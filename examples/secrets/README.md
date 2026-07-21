# secrets

Encryption-at-rest for variable values. `secrets.kdl` holds a passphrase-wrapped data key plus two
encrypted values (`db-password`, `api-token`). The plan references them like any other variable;
glidesh decrypts them in memory at run time and scrubs the plaintext from all output.

> **Demo passphrase:** `example` — this example ships with a real, committed `secrets.kdl` so you
> can run it. Never commit a real passphrase in production.

## Try it

```bash
# Read a value back (proves the round-trip)
GLIDESH_SECRET_PASS=example glidesh secret get db-password --file secrets.kdl

# Run the plan (secrets are discovered from secrets.kdl next to the inventory)
GLIDESH_SECRET_PASS=example glidesh run -i inventory.kdl -p plan.kdl
```

In the run output and the logs under `~/.glidesh/runs/`, the secret values appear as `***` — even
though the shell commands echo them — because decrypted values are redacted at the source.

## Manage the secrets

```bash
export GLIDESH_SECRET_PASS=example

glidesh secret set db-password --file secrets.kdl     # prompts for the new value
glidesh secret get db-password --file secrets.kdl     # decrypt and print
glidesh secret edit          --file secrets.kdl       # edit all values in $EDITOR
glidesh secret rekey         --file secrets.kdl        # rotate the passphrase (values unchanged)
```

## How it works

- A random 256-bit data key (DEK) encrypts each value with XChaCha20-Poly1305.
- The DEK is wrapped under a key derived from the passphrase (scrypt) and stored as `encryptedkey`.
- Each value is a short, self-describing `secret:v1:…` token — the key that decrypts it lives in
  `secrets.kdl`, so rekeying re-wraps the DEK without touching any value token.

See the [Secrets documentation](https://glidesh.dev/concepts/secrets/) for the full reference.
