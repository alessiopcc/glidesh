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

glidesh secret list          --file secrets.kdl       # what the file holds (no passphrase)
glidesh secret set db-password --file secrets.kdl     # prompts for the new value
glidesh secret get db-password --file secrets.kdl     # decrypt and print
glidesh secret rm  api-token   --file secrets.kdl     # delete a value
glidesh secret edit          --file secrets.kdl       # edit all values in $VISUAL/$EDITOR
glidesh secret rekey         --file secrets.kdl       # change the passphrase (values unchanged)
glidesh secret rekey --rotate-data-key --file secrets.kdl   # replace the key itself
```

## Sharing with a team

This example uses one shared passphrase. To give each person their own key instead, initialize with
the `age` provider and their SSH public keys — then `secret recipients add` / `rm` grants and
revokes access per person, with no shared secret to distribute:

```bash
glidesh secret init --provider age --recipient ~/.ssh/id_ed25519.pub --file team.kdl
```

See the [Secrets documentation](https://glidesh.dev/concepts/secrets/#sharing-a-vault-with-a-team).

## How it works

- A random 256-bit data key (DEK) encrypts each value with XChaCha20-Poly1305.
- The DEK is wrapped under a key derived from the passphrase (scrypt) and stored as `encryptedkey`.
- Each value is a short, self-describing `secret:v1:…` token — the key that decrypts it lives in
  `secrets.kdl`, so rekeying re-wraps the DEK without touching any value token.

See the [Secrets documentation](https://glidesh.dev/concepts/secrets/) for the full reference.
