---
title: Secrets
description: Encryption-at-rest for variable values — a pluggable provider, inline secret tokens, and automatic redaction.
---

glidesh encrypts sensitive values at rest so passwords, API keys, and TLS material can be
committed to git safely. It follows an envelope model: a single data-encryption key (DEK) is
wrapped once by a provider and stored — together with the encrypted values — in a committed
`secrets.kdl`. Individual values are short, self-describing `secret:v1:…` tokens that can appear
anywhere a variable value can.

Nothing is decrypted on target nodes; all crypto happens on the controller, and decrypted values
are scrubbed from run logs and the TUI.

## Quick start

```bash
# Create secrets.kdl and generate a wrapped data key (prompts for a passphrase)
glidesh secret init

# Encrypt a value under a key (prompts for the value with no echo if omitted)
glidesh secret set db-password
glidesh secret set api-token "sk-live-abc123"

# Read one back, and see what the file holds
glidesh secret get db-password
glidesh secret list
```

`secrets.kdl` now looks like this — safe to commit:

```kdl
secrets {
    provider "passphrase"
    encryptedkey "v1:Ro8XGHDmVVSGRwpX…"   // the wrapped data key
}

db-password "secret:v1:k6Ge72IL-UQ1lGtRm962…"
api-token   "secret:v1:-fSttta2omFd-4x9-W-d…"
```

## Using secrets in plans

Reference an encrypted variable exactly like any other variable — glidesh decrypts it in memory
at run time:

```kdl
plan "deploy" {
    step "Configure database" {
        shell "psql -c \"ALTER USER app PASSWORD '${db-password}'\""
    }
}
```

The `secrets.kdl` next to your inventory is discovered automatically. Its variables merge in at the
inventory-global tier (the lowest precedence — group, host, and plan vars override them). Override
the location with `--secrets <path>` or `$GLIDESH_SECRETS`.

You can also paste a token inline, without going through a variable — in inventory/plan
values and module arguments:

```kdl
shell "deploy --token secret:v1:-fSttta2omFd…"
```

> Inside **template file bodies** (`template=#true`), reference secrets through a variable
> (`${db-password}`) rather than pasting a raw `secret:v1:…` token — variables are decrypted
> before the template renders, whereas a literal token in file content is uploaded as-is.

### Structured secrets

A secrets file can also hold a [collection](/concepts/variables/#structured-variables) of rows,
for credentials that belong together:

```kdl
api-keys {
    - name="billing" value="secret:v1:…"
    - name="search"  value="secret:v1:…"
}
```

Every field is decrypted like any other value, so a step loop binds them as `${@item.value}` and a
template body can walk them with `${for k in api-keys}`.

Structured values are hand-written. `secret set` writes scalars only, and `secret edit` refuses a
file containing any structured node rather than risk rewriting it — so mint the tokens with
`glidesh secret encrypt` and paste them into the rows. `secret list` shows them as `structured`,
and `secret rm` removes a whole block.

## Sharing a vault with a team

The `passphrase` provider gives everyone the same secret, which has to be distributed out of band
and changed for everyone when one person leaves. The `age` provider wraps the data key to a set of
**SSH public keys** instead — the ones people already hold to reach your hosts — so access is
granted and revoked per person and nothing needs distributing.

```bash
# Anyone listed can unlock it; a .pub file or the key itself both work
glidesh secret init --provider age \
    --recipient ~/.ssh/id_ed25519.pub \
    --recipient "ssh-ed25519 AAAAC3Nza… bob@laptop"
```

```kdl
secrets {
    provider "age"
    recipients {
        - name="alice@laptop" key="ssh-ed25519 AAAAC3Nza…"
        - name="bob@laptop"   key="ssh-ed25519 AAAAC3Nza…"
    }
    encryptedkey "agev1:…"   // the data key, encrypted to both
}
```

The name comes from the key's trailing comment, which is how people already label their keys.

### Unlocking

There is no passphrase. glidesh looks for your SSH private key in this order:

1. `--secret-identity <path>`
2. `$GLIDESH_SECRET_IDENTITY`
3. the key it would use to reach the hosts (`--key`), then `~/.ssh/id_ed25519`

So by default **your host key is your vault key** and a normal run needs no extra flags. Keys must
be unencrypted, the same constraint glidesh's SSH transport has.

The wrapped key is a standard age file, base64url-encoded after the `agev1:` prefix. Decode it and
`age` reads it directly, so recovering a vault never depends on glidesh being available:

```bash
# Recover the 32-byte data key with age alone. The values are then ordinary
# XChaCha20-Poly1305 tokens under that key.
b=$(grep -o 'agev1:[A-Za-z0-9_-]*' secrets.kdl | cut -d: -f2)
while [ $(( ${#b} % 4 )) -ne 0 ]; do b="$b="; done   # restore base64 padding
printf '%s' "$b" | basenc -d --base64url | age -d -i ~/.ssh/id_ed25519 | xxd
```

### Adding and removing people

```bash
glidesh secret recipients list
glidesh secret recipients add ~/keys/carol.pub
glidesh secret recipients rm bob@laptop
```

`add` re-wraps the existing data key to the larger set, so the new person can read everything
already in the file. That is one line of diff and no one else has to do anything.

`rm` **rotates the data key by default**, re-encrypting every value under a new one. Re-wrapping
alone would not revoke anything: the removed person can still unwrap the copy of `encryptedkey`
they already have, and it opens the same key. `--keep-data-key` skips the rotation for a smaller
diff and says out loud what it costs.

The caution above still applies in full — rotation protects values written *after* it, and the old
tokens remain in your history. Removing someone is the moment to rotate the credentials themselves.

## Unlocking at run time

An age-wrapped file needs nothing here: it unlocks with your SSH key, as described above. For a
passphrase-wrapped one, provide the passphrase:

```bash
GLIDESH_SECRET_PASS=… glidesh run -i inv.kdl -p deploy.kdl              # env var (CI)
glidesh run -i inv.kdl -p deploy.kdl --secret-pass-file /run/pass       # file (CI)
glidesh run -i inv.kdl -p deploy.kdl --ask-secret-pass                  # prompt
```

Sources are consulted most-explicit-first, so a flag typed for this run always wins:

1. `--secret-pass-file <path>`
2. `$GLIDESH_SECRET_PASS`
3. `$GLIDESH_SECRET_PASS_FILE`
4. `--ask-secret-pass` (interactive prompt)

A passphrase file holds the passphrase on its **first line**; a trailing newline (`\n` or `\r\n`) is
ignored, and anything after the first line is treated as an editor artifact and skipped. Prefer a
file over the environment variable where you can — an env var is readable by every child process and
by anything that can see `/proc/<pid>/environ`. Keep it owner-only (`chmod 600`) and outside the
repository. `$GLIDESH_SECRET_PASS_FILE` is honoured by every subcommand, `glidesh secret …`
included, so CI can mount one file and use it everywhere.

A run that touches no secret needs no passphrase. If a `secret:` token is reached without a key, the
run fails with a clear message rather than proceeding.

## Redaction

Every decrypted value is tracked and scrubbed (replaced with `***`) from all emitted output —
module results, errors, the TUI, and the per-run logs in `~/.glidesh/runs/` — even if a command
echoes the secret in its own output. Explicit reveals (`secret get`) are the only path that prints
a plaintext value.

Values shorter than four characters are the one exception. Masking a one- or two-character value
would replace it everywhere it happens to occur and shred unrelated output for no real protection,
so glidesh warns instead of masking: once when `secret set` stores such a value, and again when a
run decrypts it. Short secrets are still withheld from external plugins. If a secret is short
enough to trip this, it is short enough to guess — rotate it rather than relying on redaction.

## What external plugins see

An [external module](/advanced/writing-plugins/) is a third-party executable, so it sits outside
glidesh's trust boundary and the vault is not its to read. A plugin receives only the secrets the
plan explicitly hands it:

```kdl
step "Configure the widget" {
    external "acme/widget" "app" token="${api-token}"   // explicit — the plugin gets this
}
```

Variable references and inline `secret:v1:` tokens in a task's arguments are interpolated before the
plugin is invoked, so it sees the value it was given. What it does **not** see is the rest of the
vault: the `vars` map in the request has every secret-carrying value stripped out, including values
that merely embed one — a connection string built from a password, say. A plugin that needs a secret
must be passed it explicitly.

Plugin stderr is scrubbed through the same registry before it reaches the debug log, so a plugin
that echoes a value it was handed cannot leak it through `RUST_LOG=glidesh=debug`.

## Managing secrets

| Command | What it does |
|---|---|
| `glidesh secret init` | Create `secrets.kdl`, generate and wrap a data key |
| `glidesh secret list` | List the names it holds and whether each is encrypted — no passphrase, no values |
| `glidesh secret set <key> [value]` | Encrypt a value under a key (prompts if `value` omitted) |
| `glidesh secret get <key>` | Decrypt and print one value, or a `secret:v1:…` token passed in place of a key |
| `glidesh secret rm <key>` | Delete a value (alias: `remove`) |
| `glidesh secret encrypt` | Read plaintext on stdin, print a `secret:v1:…` token |
| `glidesh secret rekey` | Re-wrap the data key under a new passphrase (value tokens unchanged) |
| `glidesh secret rekey --rotate-data-key` | Replace the data key and re-encrypt every value under it |
| `glidesh secret edit` | Open the file in `$VISUAL`/`$EDITOR` with values transiently decrypted |
| `glidesh secret recipients list` | Show who can unlock an age-wrapped file (no key needed) |
| `glidesh secret recipients add <key\|path>` | Grant access to another SSH public key |
| `glidesh secret recipients rm <name>` | Revoke a recipient, rotating the data key unless `--keep-data-key` |

All of these accept `--file <path>` (default `secrets.kdl`).

### Editing in your editor

`glidesh secret edit` decrypts every value into a temporary, owner-only file in your system temp
directory (never beside the secrets file, so a leftover can never be committed), opens it in
`$VISUAL` (falling back to `$EDITOR`, then `vi`/Notepad), and re-encrypts on save. The temp file is
shredded and removed on every exit path, including editor crashes. It handles scalar values;
add new secrets with `secret set`.

The file is edited in place rather than regenerated, so comments, ordering, and the provider block
survive the round trip — and a value you leave alone keeps its existing token instead of being
re-encrypted under a fresh nonce. Changing one secret is a one-line diff. Deleting a line in the
editor deletes that secret.

### Rotating

Three different things get called "rotation", and they answer different questions:

| What you are worried about | What to run | Diff |
|---|---|---|
| The passphrase may have leaked | `glidesh secret rekey` | one line |
| The data key may have leaked, or someone lost access | `glidesh secret rekey --rotate-data-key` | every value |
| The credential itself leaked | change it at its source, then `secret set` | the lines you changed |

`rekey` re-wraps the existing data key under a new passphrase. Only the wrapped key changes, so
every `secret:v1:…` token stays byte-identical. It is also how you strengthen a file whose key was
wrapped at a lower scrypt cost — by a development build, say — which glidesh warns about on unlock
and `validate` reports.

`rekey --rotate-data-key` also generates a fresh data key and re-encrypts every token in the file
under it — including tokens inside structured blocks, which a parser-driven rewrite would miss.
Each re-encrypted value is read back under the new key before anything is written, and the file is
replaced atomically, so an interrupted rotation cannot leave a half-rewritten vault behind.

For a passphrase-wrapped file, both ask for the current passphrase — or take it from the usual
sources — and then for a new one. To rotate the key while keeping the same passphrase, enter the
same value. For scripted use, `--new-pass-file <path>` supplies the new passphrase, since the
ordinary sources already hold the current one.

An age-wrapped file has no passphrase, so plain `rekey` has nothing to change and says so.
`rekey --rotate-data-key` rotates the key and re-wraps it to the same recipients, asking for
nothing beyond your SSH key. To change *who* can unlock it, use `secret recipients`.

:::caution[Rotation does not rewrite history]
`secrets.kdl` is committed, so the previous tokens remain in your version-control history and can
still be read by anyone holding a clone and the old passphrase — or, for an age file, a key that
was a recipient at the time. Rotating the data key protects
values written *after* the rotation. To actually revoke access to a credential, change the
credential at its source — nothing glidesh does can substitute for that.
:::

## How it works

- **Envelope encryption.** A random 256-bit DEK encrypts each value with XChaCha20-Poly1305.
  The DEK itself is wrapped by the provider and stored as `encryptedkey`.
- **Passphrase provider.** The DEK is sealed under a key derived from your passphrase with scrypt.
  A wrong passphrase fails to unwrap the DEK cleanly — the wrapped blob is its own verifier.
- **Age provider.** The DEK is encrypted to every recipient's SSH public key using the
  [age](https://age-encryption.org) format: one stanza per recipient, any one of which unwraps it.
  Adding a person re-wraps the same DEK; removing one rotates it.
- **Tokens are portable.** A `secret:v1:…` token carries only a nonce and ciphertext; the key that
  decrypts it lives in `secrets.kdl`, so tokens stay short and rekeying never rewrites them.
- **Key strength travels with the file.** Each passphrase-wrapped key records the scrypt cost it
  was written at, so an older file keeps opening. glidesh warns when it unlocks a key wrapped
  below the current default, and `glidesh validate` reports the cost, so a file created by a
  development build does not stay weak silently. `secret rekey` re-wraps it at full strength.

### What is wiped from memory

Key material is zeroized when it is dropped: the data key, the key derived from your passphrase,
and each decrypted value as it passes through the crypto layer and the redaction registry.

Decrypted values are **not** wiped once they enter the variable system. They are ordinary strings
there, the same as every other variable, and threading zeroization through the whole configuration
and executor layer would buy nothing real. A decrypted secret therefore lives in this process's
memory for the duration of the run.

None of this defends against someone who can read the process's memory, a core dump, or swap —
that is the operating system's boundary, not glidesh's. What zeroization buys is narrower and still
worth having: freed memory does not keep key material around after the code that owned it is done.
