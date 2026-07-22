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

# Read one back
glidesh secret get db-password
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

## Unlocking at run time

To decrypt during a run, provide the passphrase:

```bash
GLIDESH_SECRET_PASS=… glidesh run -i inventory.kdl -p deploy.kdl   # non-interactive (CI)
glidesh run -i inventory.kdl -p deploy.kdl --ask-secret-pass       # prompt
```

A run that touches no secret needs no passphrase. If a `secret:` token is reached without a key, the
run fails with a clear message rather than proceeding.

## Redaction

Every decrypted value is tracked and scrubbed (replaced with `***`) from all emitted output —
module results, errors, the TUI, and the per-run logs in `~/.glidesh/runs/` — even if a command
echoes the secret in its own output. Explicit reveals (`secret get`) are the only path that prints
a plaintext value.

## Managing secrets

| Command | What it does |
|---|---|
| `glidesh secret init` | Create `secrets.kdl`, generate and wrap a data key |
| `glidesh secret set <key> [value]` | Encrypt a value under a key (prompts if `value` omitted) |
| `glidesh secret get <key>` | Decrypt and print one value |
| `glidesh secret encrypt` | Read plaintext on stdin, print a `secret:v1:…` token |
| `glidesh secret rekey` | Re-wrap the data key under a new passphrase (value tokens unchanged) |
| `glidesh secret edit` | Open the file in `$EDITOR` with values transiently decrypted |

All of these accept `--file <path>` (default `secrets.kdl`).

### Editing in your editor

`glidesh secret edit` decrypts every value into a temporary, owner-only file, opens it in `$EDITOR`
(falling back to `$VISUAL`, then `vi`/Notepad), and re-encrypts on save. The temp file is shredded
and removed on every exit path, including editor crashes. It handles scalar values; add new secrets
with `secret set`.

### Rotating the passphrase

`glidesh secret rekey` re-wraps the existing data key under a new passphrase. Because only the
wrapped key changes, every `secret:v1:…` value token stays byte-identical — the rotation is a
one-line diff.

## How it works

- **Envelope encryption.** A random 256-bit DEK encrypts each value with XChaCha20-Poly1305.
  The DEK itself is wrapped by the provider and stored as `encryptedkey`.
- **Passphrase provider.** The DEK is sealed under a key derived from your passphrase with scrypt.
  A wrong passphrase fails to unwrap the DEK cleanly — the wrapped blob is its own verifier.
- **Tokens are portable.** A `secret:v1:…` token carries only a nonce and ciphertext; the key that
  decrypts it lives in `secrets.kdl`, so tokens stay short and rekeying never rewrites them.
