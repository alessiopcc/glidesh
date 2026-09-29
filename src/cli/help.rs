//! Long `--help` text. It is written to be enough on its own — for people and AI agents
//! that read `--help` and never open the docs site. The example files are macros so the
//! help can embed them with `concat!` and the tests can parse exactly what is shown.

macro_rules! example_inventory {
    () => {
        r#"vars {
    ssh-key "~/.ssh/id_ed25519"
}
group "web" {
    vars {
        port "8080"
    }
    host "web-1" "10.0.0.1" user="deploy"
    host "web-2" "10.0.0.2" user="deploy" port=2222
}
host "db-1" "10.0.1.1""#
    };
}

macro_rules! example_plan {
    () => {
        r#"plan "deploy" {
    vars {
        app-dir "/opt/app"
    }
    step "Install" {
        package "nginx"
    }
    step "Configure" {
        file "/etc/nginx/conf.d/app.conf" src="files/app.conf" template=#true mode="0644"
    }
    step "Reload" subscribe="Configure" {
        systemd "nginx" state="restarted"
    }
}"#
    };
}

macro_rules! example_rescue {
    () => {
        r#"  step "Deploy" {
      shell "deploy.sh"
      rescue {
          shell "rollback.sh"
          file "/var/log/deploy-failure.txt" src="failure.txt" template=#true
      }
      always {
          shell "rm -f /tmp/deploy.lock"
      }
  }"#
    };
}

pub const TOP_LEVEL: &str = concat!(
    "\
glidesh connects to hosts over SSH and applies a plan to them. Nothing is installed on the
hosts and no state is kept between runs: every task checks the host first and changes only
what differs, so running a plan again is safe.

Two files, both in KDL (https://kdl.dev): an inventory lists the hosts, a plan lists the
steps to apply. Relative paths in a plan resolve from the plan file's directory.

inventory.kdl:
",
    example_inventory!(),
    "

plan.kdl:
",
    example_plan!(),
    "

Typical workflow:
  glidesh validate -i inventory.kdl -p plan.kdl         check the files; connects to nothing
  glidesh run -i inventory.kdl -p plan.kdl --dry-run --diff
                                                        show what would change, and how
  glidesh run -i inventory.kdl -p plan.kdl              apply it
  glidesh logs --last                                   read the last run's per-host log

Non-interactive use (scripts, CI, agents):
  - `run` uses plain-text output when stdout is not a terminal; -T forces it.
  - Unknown host keys fail the connection: pass --accept-new-host-key to trust new hosts.
  - Encrypted secrets need a passphrase: set GLIDESH_SECRET_PASS or GLIDESH_SECRET_PASS_FILE.
  - A plan's vars-prompt is answered with --var name=value; without a terminal, a prompt
    with no --var and no default fails the run before it connects, naming the fix.
  - `run` exits non-zero when any host failed; `validate` when any check failed.

`glidesh run --help` documents plan syntax, variables, conditions and every module's
parameters. Each command's --help lists its flags.

With no command, glidesh opens the interactive console for ./inventory.kdl.
Docs: https://glidesh.netlify.app"
);

pub const RUN: &str = concat!(
    "\
Examples:
  glidesh run -i inventory.kdl -p plan.kdl
  glidesh run -i inventory.kdl -p plan.kdl -t web --dry-run --diff
  glidesh run -i inventory.kdl -p plan.kdl -t web-1,db:db-1 --serial 1 --max-fail 0
  glidesh run -i inventory.kdl -p plan.kdl --tags config --skip-tags slow
  glidesh run -i inventory.kdl                  # each host runs the plan= its inventory names
  glidesh run -i inventory.kdl -p plan.kdl --var release=v1.4   # answer a vars-prompt
  glidesh run --host 10.0.0.5 -u deploy -p plan.kdl
  glidesh run --host 10.0.0.5 -u deploy -c uptime

PLAN SYNTAX
  plan \"<name>\" [run-as=\"root\"] {
      mode \"sync\"               sync (default): all hosts finish a step before any starts the
                                next; async: each host runs at its own pace
      serial 1 \"25%\"            roll out in batches of these sizes; the last one repeats
      max-fail \"10%\"            stop starting batches once more hosts than this have failed
      vars { name \"value\" }     variables for this plan (they override inventory variables)
      vars-file \"vars.kdl\"      more variables, from a file of `name \"value\"` lines
      vars-prompt { release \"Release to deploy\" default=\"main\" }
                                variables asked for when the run starts (see PROMPTED
                                VARIABLES)
      include \"common.kdl\" [tags=\"a,b\"]
                                inline another plan's steps and variables here; tags=
                                adds those tags to every step it brings in (see TAGS)
      step \"<name>\" [attributes] { <tasks> [rescue { <tasks> }] [always { <tasks> }] }
  }
  Steps run in order; tasks in a step run in order. A step stops the host on failure,
  unless its rescue block handles it (see RESCUE AND ALWAYS).
  Step attributes:
    when=\"<condition>\"          skip the step unless the condition holds
    tags=\"web,deploy\"           select the step with --tags / --skip-tags (see TAGS)
    until=\"<command>\"           before the tasks, rerun the command on the host until it
                                exits 0 (see UNTIL); a step may be only a gate, no body
    loop=\"${var}\"               repeat for each line of var (or each row of a list
                                variable); the value is ${@item} (or ${@item.<field>})
    subscribe=\"<step>, <step>\"  when a named earlier step changed something, redo this step's
                                work (see SUBSCRIBE)
    run-as=\"root\" run-as-method=\"sudo|doas|su\"   escalate every task in the step
  Task: <module> \"<resource>\" [param=value ...] [{ param value ... }]
    Parameters go as attributes (state=\"absent\") or child nodes (state \"absent\").
    Lists: `ports \"80:80\" \"443:443\"` or a block of `- \"item\"` lines.
    Maps: a block of `key \"value\"` lines, e.g. environment { TZ \"UTC\" }.
    Booleans are #true / #false. Every task also accepts:
    when=\"<condition>\"          skip this task unless the condition holds
    register=\"var\"              store the task's trimmed output in ${var} for later steps
                                (a command run on a host keeps only the first and last
                                4 MiB of its output, so registering output over 8 MiB
                                fails the task)
    run-as=\"<user>\"             escalate this task only; run-as=\"\" opts out

VARIABLES
  ${name} expands in resources, parameters and `file` templates (template=#true). Sources,
  last wins: secrets file < inventory global vars < group vars < host vars < plan vars.
  Built in: ${@host.name} ${@host.address} ${@host.user} ${@host.port}
            ${@os.id} ${@os.version} ${@os.family} ${@os.pkg-manager} ${@os.init}
            ${@os.container-runtime} ${@os.nix-installed}
            ${@fact.hostname} ${@fact.kernel} ${@fact.arch} ${@fact.cpu.count}
            ${@fact.mem.total-mb} ${@fact.ip.default} (\"\" when the host cannot tell)
            ${@inventory.<host>.address|user|port|vars.<name>}
            ${@error.msg} ${@error.task} (in rescue and always blocks only)
  In `file` templates: ${for h in @group.web}${h.address}${endfor}, and loops over list
  variables. An undefined variable fails the task.

PROMPTED VARIABLES (vars-prompt)
  vars-prompt {
      release \"Release to deploy\" default=\"main\"
      db-password \"Database password\" secret=#true
  }
  `glidesh run` asks for each one once, before connecting to any host; the answer is a
  plan variable, ${release}. An empty answer takes default=. secret=#true reads without
  echo and shows the value as *** in output and run logs; a non-empty secret answer needs
  at least 4 bytes (4 plain ASCII characters), or it could not be masked.
  --var NAME=VALUE answers one without asking (repeatable; a name the plan does not
  prompt for is an error). When stdin is not a terminal, a prompt without --var takes its
  default, or the run fails before connecting and names every missing --var.
  Only the plan you run may prompt: vars-prompt in an included plan is an error, as is a
  prompted name that is also in vars, or one starting with `-` or with `=` or whitespace
  (--var could not name it). Inventory plan= groups whose plans prompt for the same name share one answer; it
  keeps a default only if every plan gives the same one. --dry-run asks too; `validate`
  never asks.

CONDITIONS (when=)
  ${a} == value   ${a} != value   ${a}   defined ${a}   undefined ${a}   !term   x && y   x || y
  Comparisons are string equality; single-quote values with spaces. && binds tighter than
  ||; no parentheses. Example: when=\"${@os.family} == debian && defined ${port}\"

TAGS
  --tags web,db        run only steps tagged web or db, plus steps tagged `always`
  --skip-tags slow     skip steps tagged slow; wins over --tags and `always`
  A step left out is reported as skipped, does not trigger its subscribers, and leaves its
  register= variables undefined: tag a step that registers what others need `always`.
  A tag no step carries is an error, so a typo cannot silently run nothing.
  include \"x.kdl\" tags=\"db\" tags every step the include brings in, nested includes too.

UNTIL (wait for the host to be ready)
  step \"Wait for API\" until=\"curl -sf localhost:8080/health\" until-timeout=600 until-interval=5
  Defaults: until-timeout=300, until-interval=3 (seconds). Runs once per step, before its
  tasks and loop, after when= and tags. Timing out fails the host with the command's last
  output. Waiting is not a change: it never triggers subscribers. --dry-run checks once and
  never waits. Prefer it to a polling shell with retries=, which counts as a change.

RESCUE AND ALWAYS (handle a step's failure)
",
    example_rescue!(),
    "
  rescue runs only if the step failed: a task failed, its until= timed out, or its loop=
  variable is undefined (an error in its when= is not rescued). The step's tasks stop at
  the failure. If every rescue task succeeds, the failure is handled and the host goes on.
  always runs after the step and any rescue, whether or not they failed.
  Both run once per step, after its loop, not per item, and cannot use ${@item}.
  They can read ${@error.msg} (the error) and ${@error.task} (module 'resource'; empty when
  the step failed outside its tasks), with secrets shown as ***. Never put them in a shell
  command: they hold command output and would run as shell. Render them with a file
  template, as above, or test them in when=. A failed rescue or always fails the host. A
  rescue that does not run leaves its register= variables undefined. A step's changed
  status, which subscribe= reads, counts all three blocks. A skipped step skips all three.

SUBSCRIBE
  A step with subscribe= is triggered when a step it names changed something. Triggered:
    systemd state=restarted   restarts; untriggered it only keeps the unit running (a
                              handler). Without subscribe= it restarts on every run.
    systemd state=started     restarts, so a changed config is loaded
    container running         recreated; run-once runs again despite check=
    shell                     runs despite check= (check=\"true\" = run only when triggered)
    other modules             nothing to redo: reported ok
  A triggered step that did something counts as changed and triggers its own subscribers.

MODULES (each checks the host first and changes only what differs)
  shell \"<command>\"           cmd=<string|list>  check=\"<cmd>\" (exit 0 = already done, skip)
                              retries= delay=<s> timeout=<s> success_codes=\"0,2\"
                              changed-when=#false|#true|\"<cmd>\"  login=#true
                              Runs every time unless check= says the work is done.
  file \"<dest>\"               src= (required)  template=#true  recurse=#true  fetch=#true
                              owner= group= mode=\"0644\"  diff=#false
                              Compares SHA256, then owner/group/mode. Without them a
                              replaced file keeps its owner/group/mode (run-as too), a
                              new one gets the umask mode (usually 0644).
                              With run-as, refuses a destination others could redirect:
                              a group/world-writable directory on its path (sticky /tmp
                              only above an existing dir: /tmp/app/x, not /tmp/x), or a
                              symlink owned by another user.
  package \"<name>\"            state=present|absent (apt, dnf, yum, pacman, apk, zypper, nix)
  user \"<name>\"               uid= shell= groups=\"a,b\" state=present|absent
  systemd \"<unit>\"            state=started|stopped|restarted  enabled=#true|#false
                              command= (creates the unit) description= user= group=
                              working-dir= restart-policy= type= after= wanted-by=
                              environment { K \"V\" }
  container \"<name>\"          image= state=running|stopped|absent|run-once  runtime=docker|podman
                              ports volumes environment labels network command entrypoint
                              restart pull user workdir gpus devices memory cpus
                              cap-add cap-drop security-opt extra-args healthcheck { }
                              wait=healthy|running ready-cmd= wait-timeout= (and more)
                              Recreated only when a parameter changed: the hash of all of
                              them is kept in the sh.glide.param-hash label. A stopped
                              container that still matches is started.
  disk \"<device>\"             fs= (required) mount= (required) opts= force=#true
                              state=mounted|unmounted|absent
  nix \"<package>\"             action=install|shell|build|channel|flake-update|gc
                              state= profile= packages= url= update= install=#true
  host \"<label>\"              cmd= on=\"<inventory host>\" -- runs once for all hosts, on the
                              controller or on one host, and shares register= with every host
  external \"<plugin>\" \"<res>\" a plugin from ./modules/ or ~/.glidesh/modules/

PREVIEW AND OUTPUT
  --dry-run runs every check and nothing else; each task reports `ok` or `would change`
  with the reason. --diff adds the detail: a unified diff of file content, the container
  parameters that changed. Probes still run in a preview: shell check= commands, container
  readiness gates. Each host's log is kept under ~/.glidesh/runs/ (see `glidesh logs`).
  Exit status is non-zero when any host failed or a rolling run stopped early.

SSH
  The key is --key, else the inventory's ssh-key variable, else ~/.ssh/id_ed25519. Host
  keys are checked against ~/.ssh/known_hosts. Hosts behind a bastion use a `jump \"<addr>\"`
  child node on their group or host in the inventory.

Docs: https://glidesh.netlify.app/cli/#glidesh-run"
);

#[cfg(test)]
mod tests {
    /// The examples in `glidesh --help` are what an agent will copy; they must parse.
    #[test]
    fn the_help_examples_parse() {
        glidesh::config::parse_inventory(example_inventory!()).unwrap();
        glidesh::config::parse_plan(example_plan!()).unwrap();
    }

    #[test]
    fn the_rescue_example_parses() {
        let plan = format!("plan \"p\" {{\n{}\n}}", example_rescue!());
        let plan = glidesh::config::parse_plan(&plan).unwrap();
        let step = plan.steps()[0];
        assert_eq!((step.rescue.len(), step.always.len()), (2, 1));
    }

    #[test]
    fn the_help_states_the_output_limit() {
        let mib = glidesh::ssh::connection::OUTPUT_LIMIT / (1024 * 1024);
        assert!(super::RUN.contains(&format!("output over {mib} MiB")));
        assert!(super::RUN.contains(&format!("{} MiB of its output", mib / 2)));
    }
}
