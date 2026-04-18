#![allow(clippy::needless_return, clippy::collapsible_match)]

use crate::tui::tunnel_store::{self, SavedTunnel};
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use glidesh::config::types::{Inventory, ResolvedHost};
use glidesh::error::GlideshError;
use glidesh::ssh::HostKeyPolicy;
use glidesh::ssh::session_pool::SessionPool;
use glidesh::ssh::tunnel::{
    LocalForward, ReverseForward, TunnelDirection, start_local_forward, start_reverse_forward,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Tunnels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogField {
    LocalPort,
    RemoteHost,
    RemotePort,
    Reverse,
    Save,
}

struct TunnelDialog {
    local_port: String,
    remote_host: String,
    remote_port: String,
    reverse: bool,
    save: bool,
    field: DialogField,
    error: Option<String>,
}

impl Default for TunnelDialog {
    fn default() -> Self {
        Self {
            local_port: String::new(),
            remote_host: String::new(),
            remote_port: String::new(),
            reverse: false,
            save: false,
            field: DialogField::LocalPort,
            error: None,
        }
    }
}

#[derive(Debug, Clone)]
enum TreeRow {
    Group(usize),       // group_idx
    Host(usize, usize), // group_idx, host_idx_in_group
}

struct GroupView {
    name: String,
    host_idxs: Vec<usize>, // indices into ConsoleState.hosts
    expanded: bool,
    plan: Option<String>,
    is_real: bool, // false for the "(ungrouped)" pseudo-group
}

enum TunnelKind {
    Local(LocalForward),
    Reverse(ReverseForward),
}

impl TunnelKind {
    fn accepts(&self) -> &Arc<AtomicUsize> {
        match self {
            TunnelKind::Local(l) => &l.accepts,
            TunnelKind::Reverse(r) => &r.accepts,
        }
    }
}

struct TunnelEntry {
    direction: TunnelDirection,
    via_host_name: String, // for display + saved spec key
    local_port: u16,
    remote_host: String,
    remote_port: u16,
    saved: bool,
    status: String,
    kind: Option<TunnelKind>,
}

impl TunnelEntry {
    fn accept_count(&self) -> usize {
        self.kind
            .as_ref()
            .map(|k| k.accepts().load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

struct ConsoleState {
    inventory_path: PathBuf,
    hosts: Vec<ResolvedHost>,
    host_plans: Vec<Option<String>>, // parallel to `hosts`
    groups: Vec<GroupView>,
    rows: Vec<TreeRow>,
    cursor: usize,
    selection: HashSet<usize>, // host indices
    focus: Focus,
    tunnel_cursor: usize,
    tunnels: Vec<TunnelEntry>,
    dialog: Option<TunnelDialog>,
    confirm_quit: bool,
    confirm_kill: Option<usize>,
    flash: Option<(String, std::time::Instant)>,
    ssh_key_path: PathBuf,
    host_key_policy: HostKeyPolicy,
}

impl ConsoleState {
    fn new(
        inventory_path: PathBuf,
        inv: &Inventory,
        ssh_key_path: PathBuf,
        host_key_policy: HostKeyPolicy,
    ) -> Self {
        let mut hosts: Vec<ResolvedHost> = Vec::new();
        let mut host_plans: Vec<Option<String>> = Vec::new();
        let mut groups: Vec<GroupView> = Vec::new();

        let all = inv.resolve_targets(None);
        let mut seen: HashSet<String> = HashSet::new();
        for group in &inv.groups {
            let mut idxs = Vec::new();
            for h in &group.hosts {
                if let Some((i, _)) = all.iter().enumerate().find(|(_, rh)| rh.name == h.name) {
                    if seen.insert(h.name.clone()) {
                        hosts.push(all[i].clone());
                        host_plans.push(h.plan.clone());
                        idxs.push(hosts.len() - 1);
                    }
                }
            }
            groups.push(GroupView {
                name: group.name.clone(),
                host_idxs: idxs,
                expanded: true,
                plan: group.plan.clone(),
                is_real: true,
            });
        }
        let mut ungrouped_idxs = Vec::new();
        for h in &inv.ungrouped_hosts {
            if let Some((i, _)) = all.iter().enumerate().find(|(_, rh)| rh.name == h.name) {
                if seen.insert(h.name.clone()) {
                    hosts.push(all[i].clone());
                    host_plans.push(h.plan.clone());
                    ungrouped_idxs.push(hosts.len() - 1);
                }
            }
        }
        if !ungrouped_idxs.is_empty() {
            groups.push(GroupView {
                name: "(ungrouped)".to_string(),
                host_idxs: ungrouped_idxs,
                expanded: true,
                plan: None,
                is_real: false,
            });
        }

        let mut state = Self {
            inventory_path,
            hosts,
            host_plans,
            groups,
            rows: Vec::new(),
            cursor: 0,
            selection: HashSet::new(),
            focus: Focus::Tree,
            tunnel_cursor: 0,
            tunnels: Vec::new(),
            dialog: None,
            confirm_quit: false,
            confirm_kill: None,
            flash: None,
            ssh_key_path,
            host_key_policy,
        };
        state.recompute_rows();
        state
    }

    /// Plan associated with the cursor row, plus the target filter to pass to `glidesh run`.
    fn current_plan(&self) -> Option<(String, String)> {
        match self.rows.get(self.cursor)? {
            TreeRow::Group(gi) => {
                let g = &self.groups[*gi];
                let plan = g.plan.clone()?;
                Some((plan, g.name.clone()))
            }
            TreeRow::Host(gi, hi) => {
                let host_idx = self.groups[*gi].host_idxs.get(*hi).copied()?;
                if let Some(plan) = self.host_plans.get(host_idx).cloned().flatten() {
                    return Some((plan, self.hosts[host_idx].name.clone()));
                }
                let g = &self.groups[*gi];
                if !g.is_real {
                    return None;
                }
                let plan = g.plan.clone()?;
                Some((plan, format!("{}:{}", g.name, self.hosts[host_idx].name)))
            }
        }
    }

    fn recompute_rows(&mut self) {
        self.rows.clear();
        for (gi, g) in self.groups.iter().enumerate() {
            self.rows.push(TreeRow::Group(gi));
            if g.expanded {
                for (hi, _) in g.host_idxs.iter().enumerate() {
                    self.rows.push(TreeRow::Host(gi, hi));
                }
            }
        }
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    fn current_host_idx(&self) -> Option<usize> {
        match self.rows.get(self.cursor)? {
            TreeRow::Host(gi, hi) => self.groups[*gi].host_idxs.get(*hi).copied(),
            TreeRow::Group(_) => None,
        }
    }

    fn selected_host_idxs(&self) -> Vec<usize> {
        if !self.selection.is_empty() {
            let mut v: Vec<_> = self.selection.iter().copied().collect();
            v.sort();
            return v;
        }
        match self.rows.get(self.cursor) {
            Some(TreeRow::Host(gi, hi)) => {
                if let Some(&idx) = self.groups[*gi].host_idxs.get(*hi) {
                    return vec![idx];
                }
                Vec::new()
            }
            Some(TreeRow::Group(gi)) => self.groups[*gi].host_idxs.clone(),
            None => Vec::new(),
        }
    }

    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), std::time::Instant::now()));
    }

    fn flash_text(&self) -> Option<&str> {
        let (msg, when) = self.flash.as_ref()?;
        if when.elapsed() > Duration::from_secs(5) {
            None
        } else {
            Some(msg.as_str())
        }
    }
}

pub async fn run(
    inventory_path: &Path,
    inventory: &Inventory,
    ssh_key: PrivateKeyWithHashAlg,
    ssh_key_path: PathBuf,
    policy: HostKeyPolicy,
) -> io::Result<()> {
    let mut state = ConsoleState::new(
        inventory_path.to_path_buf(),
        inventory,
        ssh_key_path,
        policy,
    );
    let pool = Arc::new(SessionPool::new(ssh_key.clone(), policy));

    // Auto-open saved tunnels.
    let saved = tunnel_store::load(inventory_path);
    for spec in saved {
        let host_idx = state.hosts.iter().position(|h| h.name == spec.via);
        let Some(host_idx) = host_idx else {
            state.tunnels.push(TunnelEntry {
                direction: spec.direction,
                via_host_name: spec.via.clone(),
                local_port: spec.local_port,
                remote_host: spec.remote_host.clone(),
                remote_port: spec.remote_port,
                saved: true,
                status: format!("Host '{}' not in inventory", spec.via),
                kind: None,
            });
            continue;
        };
        let host = state.hosts[host_idx].clone();
        match open_tunnel(&pool, &host, &spec).await {
            Ok(kind) => {
                state.tunnels.push(TunnelEntry {
                    direction: spec.direction,
                    via_host_name: host.name.clone(),
                    local_port: spec.local_port,
                    remote_host: spec.remote_host.clone(),
                    remote_port: spec.remote_port,
                    saved: true,
                    status: "active".to_string(),
                    kind: Some(kind),
                });
            }
            Err(e) => {
                state.tunnels.push(TunnelEntry {
                    direction: spec.direction,
                    via_host_name: host.name.clone(),
                    local_port: spec.local_port,
                    remote_host: spec.remote_host.clone(),
                    remote_port: spec.remote_port,
                    saved: true,
                    status: format!("error: {}", e),
                    kind: None,
                });
            }
        }
    }

    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut state, &pool, &ssh_key, policy).await;

    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    // Cancel any non-saved tunnels on exit (saved ones we still cancel
    // at runtime — they'll reopen on next launch from the spec file).
    for t in &state.tunnels {
        if let Some(kind) = &t.kind {
            match kind {
                TunnelKind::Local(l) => l.cancel(),
                TunnelKind::Reverse(r) => r.cancel().await,
            }
        }
    }

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut ConsoleState,
    pool: &Arc<SessionPool>,
    ssh_key: &PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| render(f, state))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Confirm dialogs take precedence.
        if state.confirm_quit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(()),
                _ => state.confirm_quit = false,
            }
            continue;
        }
        if let Some(idx) = state.confirm_kill {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    kill_tunnel(state, idx, true).await;
                    state.confirm_kill = None;
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    kill_tunnel(state, idx, false).await;
                    state.confirm_kill = None;
                }
                KeyCode::Esc => state.confirm_kill = None,
                _ => {}
            }
            continue;
        }

        // Tunnel dialog.
        if state.dialog.is_some() {
            handle_dialog_key(state, key, pool).await;
            continue;
        }

        // Global keys.
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if state.tunnels.iter().any(|t| t.kind.is_some()) {
                    state.confirm_quit = true;
                } else {
                    return Ok(());
                }
            }
            KeyCode::Char('q') => {
                if state.tunnels.iter().any(|t| t.kind.is_some()) {
                    state.confirm_quit = true;
                } else {
                    return Ok(());
                }
            }
            KeyCode::Tab => {
                state.focus = match state.focus {
                    Focus::Tree => Focus::Tunnels,
                    Focus::Tunnels => Focus::Tree,
                };
            }
            _ => match state.focus {
                Focus::Tree => handle_tree_key(state, key, pool, ssh_key, policy, terminal).await?,
                Focus::Tunnels => handle_tunnels_key(state, key).await,
            },
        }
    }
}

async fn handle_tree_key(
    state: &mut ConsoleState,
    key: KeyEvent,
    pool: &Arc<SessionPool>,
    ssh_key: &PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.cursor + 1 < state.rows.len() {
                state.cursor += 1;
            }
        }
        KeyCode::Left => {
            if let Some(TreeRow::Group(gi)) = state.rows.get(state.cursor) {
                state.groups[*gi].expanded = false;
                state.recompute_rows();
            } else if let Some(TreeRow::Host(gi, _)) = state.rows.get(state.cursor).cloned() {
                state.groups[gi].expanded = false;
                // move cursor to group row
                let gi_val = gi;
                for (i, r) in state.rows.iter().enumerate() {
                    if matches!(r, TreeRow::Group(g) if *g == gi_val) {
                        state.cursor = i;
                        break;
                    }
                }
                state.recompute_rows();
            }
        }
        KeyCode::Right => {
            if let Some(TreeRow::Group(gi)) = state.rows.get(state.cursor) {
                state.groups[*gi].expanded = true;
                state.recompute_rows();
            }
        }
        KeyCode::Char(' ') => {
            // Toggle selection of the row (host or group → all its hosts).
            match state.rows.get(state.cursor).cloned() {
                Some(TreeRow::Host(gi, hi)) => {
                    if let Some(&idx) = state.groups[gi].host_idxs.get(hi) {
                        if state.selection.contains(&idx) {
                            state.selection.remove(&idx);
                        } else {
                            state.selection.insert(idx);
                        }
                    }
                }
                Some(TreeRow::Group(gi)) => {
                    let idxs = state.groups[gi].host_idxs.clone();
                    let all_selected = idxs.iter().all(|i| state.selection.contains(i));
                    if all_selected {
                        for i in &idxs {
                            state.selection.remove(i);
                        }
                    } else {
                        for i in idxs {
                            state.selection.insert(i);
                        }
                    }
                }
                None => {}
            }
        }
        KeyCode::Esc => {
            state.selection.clear();
        }
        KeyCode::Enter | KeyCode::Char('s') => {
            let targets = state.selected_host_idxs();
            if targets.is_empty() {
                state.set_flash("no targets; select a host or group");
                return Ok(());
            }
            let hosts: Vec<ResolvedHost> =
                targets.iter().map(|&i| state.hosts[i].clone()).collect();
            // Tear down TUI, launch shell, restore.
            terminal::disable_raw_mode()?;
            io::stdout().execute(LeaveAlternateScreen)?;

            let result = if hosts.len() == 1 {
                open_single_shell(pool, &hosts[0]).await
            } else {
                crate::tui::shell_tui::run_shell_tui(&hosts, ssh_key, policy, 10)
                    .await
                    .map_err(|e| GlideshError::Other(format!("{}", e)))
            };

            terminal::enable_raw_mode()?;
            io::stdout().execute(EnterAlternateScreen)?;
            terminal.clear()?;

            if let Err(e) = result {
                state.set_flash(format!("shell error: {}", e));
            }
        }
        KeyCode::Char('t') => {
            let Some(host_idx) = state.current_host_idx() else {
                state.set_flash("move cursor to a host to open a tunnel");
                return Ok(());
            };
            if state.selection.len() > 1 {
                state.set_flash("tunnels require a single host (clear selection with Esc)");
                return Ok(());
            }
            let _ = host_idx;
            state.dialog = Some(TunnelDialog::default());
        }
        KeyCode::Char('r') => {
            run_plan(state, terminal)?;
        }
        _ => {}
    }
    Ok(())
}

fn run_plan(
    state: &mut ConsoleState,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    let Some((plan_rel, target)) = state.current_plan() else {
        state.set_flash("no plan associated with this row");
        return Ok(());
    };
    let inv_dir = state
        .inventory_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let plan_path = if Path::new(&plan_rel).is_absolute() {
        PathBuf::from(&plan_rel)
    } else {
        inv_dir.join(&plan_rel)
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            state.set_flash(format!("locate exe: {}", e));
            return Ok(());
        }
    };

    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("run")
        .arg("-i")
        .arg(&state.inventory_path)
        .arg("-p")
        .arg(&plan_path)
        .arg("-t")
        .arg(&target)
        .arg("-k")
        .arg(&state.ssh_key_path);
    if !state.host_key_policy.verify {
        cmd.arg("--no-host-key-check");
    }
    if state.host_key_policy.accept_new {
        cmd.arg("--accept-new-host-key");
    }
    let status = cmd.status();

    println!("\nPress any key to return to console...\r");
    let _ = event::read();

    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    terminal.clear()?;

    match status {
        Ok(s) if s.success() => state.set_flash(format!("plan '{}' completed", plan_rel)),
        Ok(s) => state.set_flash(format!("plan '{}' exited: {}", plan_rel, s)),
        Err(e) => state.set_flash(format!("failed to spawn run: {}", e)),
    }
    Ok(())
}

async fn handle_tunnels_key(state: &mut ConsoleState, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if state.tunnel_cursor > 0 {
                state.tunnel_cursor -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.tunnel_cursor + 1 < state.tunnels.len() {
                state.tunnel_cursor += 1;
            }
        }
        KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d') | KeyCode::Char('x') => {
            if let Some(t) = state.tunnels.get(state.tunnel_cursor) {
                if t.saved {
                    state.confirm_kill = Some(state.tunnel_cursor);
                } else {
                    let idx = state.tunnel_cursor;
                    kill_tunnel(state, idx, false).await;
                }
            }
        }
        _ => {}
    }
}

async fn handle_dialog_key(state: &mut ConsoleState, key: KeyEvent, pool: &Arc<SessionPool>) {
    let Some(dialog) = state.dialog.as_mut() else {
        return;
    };

    match key.code {
        KeyCode::Esc => {
            state.dialog = None;
            return;
        }
        KeyCode::Tab => {
            dialog.field = match dialog.field {
                DialogField::LocalPort => DialogField::RemoteHost,
                DialogField::RemoteHost => DialogField::RemotePort,
                DialogField::RemotePort => DialogField::Reverse,
                DialogField::Reverse => DialogField::Save,
                DialogField::Save => DialogField::LocalPort,
            };
            return;
        }
        KeyCode::BackTab => {
            dialog.field = match dialog.field {
                DialogField::LocalPort => DialogField::Save,
                DialogField::RemoteHost => DialogField::LocalPort,
                DialogField::RemotePort => DialogField::RemoteHost,
                DialogField::Reverse => DialogField::RemotePort,
                DialogField::Save => DialogField::Reverse,
            };
            return;
        }
        KeyCode::Char(' ') if matches!(dialog.field, DialogField::Reverse | DialogField::Save) => {
            match dialog.field {
                DialogField::Reverse => dialog.reverse = !dialog.reverse,
                DialogField::Save => dialog.save = !dialog.save,
                _ => {}
            }
            return;
        }
        KeyCode::Enter => {
            let local_port: u16 = match dialog.local_port.trim().parse() {
                Ok(v) if v > 0 => v,
                _ => {
                    dialog.error = Some("invalid local port".to_string());
                    return;
                }
            };
            let remote_host = dialog.remote_host.trim().to_string();
            if remote_host.is_empty() {
                dialog.error = Some("remote host required".to_string());
                return;
            }
            let remote_port: u16 = match dialog.remote_port.trim().parse() {
                Ok(v) if v > 0 => v,
                _ => {
                    dialog.error = Some("invalid remote port".to_string());
                    return;
                }
            };
            let reverse = dialog.reverse;
            let save = dialog.save;
            let direction = if reverse {
                TunnelDirection::Reverse
            } else {
                TunnelDirection::Local
            };

            // Release mut borrow on dialog before touching state.
            let _ = dialog;
            let host_idx = match state.current_host_idx() {
                Some(i) => i,
                None => {
                    if let Some(d) = state.dialog.as_mut() {
                        d.error = Some("cursor not on a host".to_string());
                    }
                    return;
                }
            };
            let host = state.hosts[host_idx].clone();
            let spec = SavedTunnel {
                direction,
                via: host.name.clone(),
                local_port,
                remote_host: remote_host.clone(),
                remote_port,
            };

            match open_tunnel(pool, &host, &spec).await {
                Ok(kind) => {
                    state.tunnels.push(TunnelEntry {
                        direction,
                        via_host_name: host.name.clone(),
                        local_port,
                        remote_host: remote_host.clone(),
                        remote_port,
                        saved: save,
                        status: "active".to_string(),
                        kind: Some(kind),
                    });
                    if save {
                        if let Err(e) = tunnel_store::upsert(&state.inventory_path, spec) {
                            state.set_flash(format!("tunnel saved but disk write failed: {}", e));
                        }
                    }
                    state.dialog = None;
                    state.set_flash("tunnel opened");
                }
                Err(e) => {
                    if let Some(d) = state.dialog.as_mut() {
                        d.error = Some(format!("{}", e));
                    }
                }
            }
            return;
        }
        KeyCode::Backspace => {
            match dialog.field {
                DialogField::LocalPort => {
                    dialog.local_port.pop();
                }
                DialogField::RemoteHost => {
                    dialog.remote_host.pop();
                }
                DialogField::RemotePort => {
                    dialog.remote_port.pop();
                }
                _ => {}
            }
            return;
        }
        KeyCode::Char(c) => match dialog.field {
            DialogField::LocalPort => {
                if c.is_ascii_digit() {
                    dialog.local_port.push(c);
                }
            }
            DialogField::RemoteHost => {
                dialog.remote_host.push(c);
            }
            DialogField::RemotePort => {
                if c.is_ascii_digit() {
                    dialog.remote_port.push(c);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

async fn open_tunnel(
    pool: &Arc<SessionPool>,
    host: &ResolvedHost,
    spec: &SavedTunnel,
) -> Result<TunnelKind, GlideshError> {
    let session = pool.get_or_connect(host).await?;
    match spec.direction {
        TunnelDirection::Local => {
            let lf = start_local_forward(
                Arc::clone(&session),
                host.name.clone(),
                spec.local_port,
                spec.remote_host.clone(),
                spec.remote_port,
            )
            .await?;
            Ok(TunnelKind::Local(lf))
        }
        TunnelDirection::Reverse => {
            let rf = start_reverse_forward(
                Arc::clone(&session),
                host.name.clone(),
                spec.remote_port, // sshd binds this port on remote
                spec.remote_host.clone(),
                spec.local_port,
            )
            .await?;
            Ok(TunnelKind::Reverse(rf))
        }
    }
}

async fn kill_tunnel(state: &mut ConsoleState, idx: usize, remove_saved: bool) {
    let Some(entry) = state.tunnels.get_mut(idx) else {
        return;
    };
    if let Some(kind) = entry.kind.take() {
        match kind {
            TunnelKind::Local(l) => l.cancel(),
            TunnelKind::Reverse(r) => r.cancel().await,
        }
    }
    let was_saved = entry.saved;
    let direction = entry.direction;
    let via = entry.via_host_name.clone();
    let local_port = entry.local_port;
    state.tunnels.remove(idx);
    if state.tunnel_cursor >= state.tunnels.len() && !state.tunnels.is_empty() {
        state.tunnel_cursor = state.tunnels.len() - 1;
    }
    if was_saved && remove_saved {
        if let Err(e) = tunnel_store::remove(&state.inventory_path, direction, &via, local_port) {
            state.set_flash(format!("saved spec removal failed: {}", e));
        }
    }
}

async fn open_single_shell(
    pool: &Arc<SessionPool>,
    host: &ResolvedHost,
) -> Result<(), GlideshError> {
    let session = pool.get_or_connect(host).await?;
    println!(
        "Connected to {}@{}. Type 'exit' to return to console.\r",
        host.user, host.address
    );
    let _ = session.interactive_shell().await?;
    println!("\r\nShell exited.\r");
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}

// ---------- Rendering ----------

fn render(frame: &mut ratatui::Frame, state: &ConsoleState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(55),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(chunks[1]);

    render_header(frame, chunks[0], state);
    render_tree(frame, top[0], state);
    render_plan_panel(frame, top[1], state);
    render_tunnels(frame, chunks[2], state);
    render_footer(frame, chunks[3], state);

    if state.dialog.is_some() {
        render_dialog(frame, area, state);
    }
    if state.confirm_quit {
        render_confirm(frame, area, "Close active tunnels and quit? (y/N)");
    }
    if state.confirm_kill.is_some() {
        render_confirm(
            frame,
            area,
            "Kill saved tunnel — also delete saved spec? (y = delete / n = keep / Esc = cancel)",
        );
    }
}

fn render_header(frame: &mut ratatui::Frame, area: Rect, state: &ConsoleState) {
    let title = format!(
        " glidesh console — {}  [{} hosts, {} tunnels]",
        state.inventory_path.display(),
        state.hosts.len(),
        state.tunnels.len(),
    );
    frame.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn render_tree(frame: &mut ratatui::Frame, area: Rect, state: &ConsoleState) {
    let title = if state.focus == Focus::Tree {
        " Hosts [focused] "
    } else {
        " Hosts "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<ListItem> = Vec::new();
    for (ri, row) in state.rows.iter().enumerate() {
        let is_cursor = ri == state.cursor && state.focus == Focus::Tree;
        let line = match row {
            TreeRow::Group(gi) => {
                let g = &state.groups[*gi];
                let chevron = if g.expanded { "▾" } else { "▸" };
                let count = g.host_idxs.len();
                Line::from(vec![
                    Span::raw(format!("{} {} ", chevron, g.name)),
                    Span::styled(format!("({})", count), Style::default().fg(Color::DarkGray)),
                ])
            }
            TreeRow::Host(gi, hi) => {
                let idx = state.groups[*gi].host_idxs[*hi];
                let host = &state.hosts[idx];
                let selected = state.selection.contains(&idx);
                let marker = if selected { "✓" } else { " " };
                Line::from(vec![
                    Span::raw(format!("   [{}] ", marker)),
                    Span::raw(host.name.clone()),
                    Span::styled(
                        format!("  {}@{}:{}", host.user, host.address, host.port),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            }
        };
        let style = if is_cursor {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(ListItem::new(line).style(style));
    }
    frame.render_widget(List::new(lines), inner);
}

fn render_tunnels(frame: &mut ratatui::Frame, area: Rect, state: &ConsoleState) {
    let title = if state.focus == Focus::Tunnels {
        " Tunnels [focused] "
    } else {
        " Tunnels "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header = Row::new(vec![
        "Dir", "Local", "Via", "Remote", "Accepts", "Saved", "Status",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = state
        .tunnels
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let dir = match t.direction {
                TunnelDirection::Local => "L",
                TunnelDirection::Reverse => "R",
            };
            let saved = if t.saved { "✓" } else { " " };
            let style = if i == state.tunnel_cursor && state.focus == Focus::Tunnels {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(dir),
                Cell::from(format!("127.0.0.1:{}", t.local_port)),
                Cell::from(t.via_host_name.clone()),
                Cell::from(format!("{}:{}", t.remote_host, t.remote_port)),
                Cell::from(t.accept_count().to_string()),
                Cell::from(saved),
                Cell::from(t.status.clone()),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(4),
        Constraint::Length(20),
        Constraint::Length(18),
        Constraint::Length(24),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Min(10),
    ];
    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, inner);
}

fn render_plan_panel(frame: &mut ratatui::Frame, area: Rect, state: &ConsoleState) {
    let block = Block::default().borders(Borders::ALL).title(" Plan ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = match state.current_plan() {
        Some((plan, target)) => vec![
            Line::from(vec![
                Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
                Span::styled(target, Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("Plan:   ", Style::default().fg(Color::DarkGray)),
                Span::raw(plan),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Press r to run",
                Style::default().fg(Color::Yellow),
            )),
        ],
        None => match state.rows.get(state.cursor) {
            Some(_) => vec![
                Line::from(Span::styled(
                    "(no plan associated)",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Set plan=\"...\" on the group or",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "host in the inventory.",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            None => vec![Line::from("")],
        },
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &ConsoleState) {
    let base = "↑↓ nav  Space select  Enter/s shell  t tunnel  r run  Tab focus  d kill  q quit";
    let text = if let Some(msg) = state.flash_text() {
        format!(" {}  |  {}", base, msg)
    } else {
        format!(" {}", base)
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn render_dialog(frame: &mut ratatui::Frame, area: Rect, state: &ConsoleState) {
    let Some(d) = state.dialog.as_ref() else {
        return;
    };
    let w = 60u16.min(area.width.saturating_sub(4));
    let h = 13u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" New tunnel ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let fields = [
        ("Local port ", &d.local_port, DialogField::LocalPort, false),
        (
            "Remote host",
            &d.remote_host,
            DialogField::RemoteHost,
            false,
        ),
        (
            "Remote port",
            &d.remote_port,
            DialogField::RemotePort,
            false,
        ),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (label, val, f, _) in &fields {
        let focus = d.field == *f;
        let cursor = if focus { "_" } else { "" };
        let style = if focus {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(format!("  {} : ", label)),
            Span::styled(format!("{}{}", val, cursor), style),
        ]));
    }
    let rev_focus = d.field == DialogField::Reverse;
    let rev_mark = if d.reverse { "✓" } else { " " };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("[{}] Reverse (-R)", rev_mark),
            if rev_focus {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            },
        ),
    ]));
    let save_focus = d.field == DialogField::Save;
    let save_mark = if d.save { "✓" } else { " " };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("[{}] Save (auto-open next time)", save_mark),
            if save_focus {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            },
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Tab cycle  Space toggle  Enter submit  Esc cancel",
        Style::default().fg(Color::DarkGray),
    )));
    if let Some(err) = &d.error {
        lines.push(Line::from(Span::styled(
            format!("  {}", err),
            Style::default().fg(Color::Red),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_confirm(frame: &mut ratatui::Frame, area: Rect, text: &str) {
    let w = (text.len() as u16 + 4).min(area.width.saturating_sub(4));
    let h = 3u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" Confirm ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(text), inner);
}
