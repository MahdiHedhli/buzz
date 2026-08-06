# Buzz shared compute: local GUI verification

This runbook verifies the actual desktop path used by the built-in **Fizz** agent:

`Buzz Desktop → buzz-acp → buzz-agent → MeshLLM SDK → local/remote compute`

It does not use a substitute agent harness.

## Before starting

Run from the `block/buzz` repository root on the mesh-enabled branch.

For a completely fresh, deterministic local state, use:

```bash
. ./bin/activate-hermit
just mesh-dev-fresh
```

This removes development app data, the development keyring entry,
`~/.buzz-dev`, and local Docker volumes; it preserves the installed Buzz app's
data, production keyring, and `~/.buzz`. The first dev page load also clears
only that dev server origin's WebKit storage, so saved fields from an earlier
run cannot leak into the fresh state. It then seeds local channels and starts
the mesh-enabled desktop with the repository's public Tyler test identity.
That identity is a fixture and must never be pointed at staging or production.

If using `mesh-dev-fresh`, the clean window opens at **Welcome to Buzz**. Join
the seeded local community before continuing:

1. Click **Join a community**.
2. Use any local name, such as **Local Buzz**.
3. Set **Community URL** to `ws://localhost:3000` and join.
4. Complete the short profile setup if it appears.

The recipe already supplied the repository's public test identity and seeded
the local channels. Do not import or generate another key. Continue at **Share
this machine** below.

Free the development ports if a previous run was interrupted:

```bash
lsof -nP -iTCP:3000 -iTCP:8080 -iTCP:9102 -iTCP:9337 -iTCP:3131
```

Stop only stale Buzz/MeshLLM processes shown by that command. Do not leave a
standalone `mesh-llm` process using `9337` or `3131`; the desktop owns those
ports during this test.

## 1. Launch the mesh-enabled desktop

```bash
. ./bin/activate-hermit
just mesh=1 dev
```

Keep that terminal open. The first run may build/install the native runtime and
take several minutes. Wait for the Buzz window to open and for the terminal to
stop printing build progress.

Using plain `just dev` is not sufficient: the Compute UI and embedded MeshLLM
runtime are behind the `mesh-llm` feature.

## 2. Share this machine

1. Open **Settings**.
2. Select **Compute**.
3. Under **Share compute**, choose a suggested model.
   - On a 16 GB Apple Silicon machine, use a suggested Qwen3.5 4B quantized
     model when available.
   - `unsloth/Qwen3.5-4B-GGUF:Q4_K_M` is the model used by the hardware proof.
   - Do not use a sub-1B model for the channel-reply proof. It can prove that
     inference is reachable while still failing the agent's long prompt and
     required message-send tool call.
4. Turn on **Share this machine**.
5. Wait until the card says it is sharing/running. Do not start Fizz while the
   card says downloading, preparing, or starting.

Buzz may download the model on first use. The model picker ranks models for the
current hardware; avoid entering a model the card marks too large.

## 3. Make shared compute the agent default

1. Open **Agents** from the left sidebar.
2. In **Agent defaults**, set **Default LLM provider** to
   **Buzz shared compute**.
3. Set **Default model** to **Default (auto)**.
4. Click **Save defaults** and wait for **Saved**.

Fizz has no pinned runtime/provider/model, so it inherits these defaults and
resolves to the bundled `buzz-agent`. No API key is required.

## 4. Start the real Fizz path

1. Find the **Fizz** card on the Agents screen.
2. If Fizz is stopped, click the small play badge over its avatar. If it is
   running, the badge is a green status dot instead of a stop control.
3. Wait for its runtime indicator to become active.
4. Add Fizz to a channel if it is not already a channel member.
5. In that channel, send:

   ```text
   @Fizz Reply exactly: FIZZ_MESH_OK
   ```

6. Confirm that Fizz replies `FIZZ_MESH_OK` in the channel.

That channel response is the end-to-end proof. A green Compute card alone proves
only model serving; it does not prove the Fizz harness and provider inheritance.

To stop a running agent, click the body/name of its card to open its profile,
then click **Stop** near the top. The green avatar badge is status-only while the
agent is running. Once stopped, the profile action becomes **Respawn** and the
avatar badge becomes a play button.

To create a separate test agent, choose **New agent → New agent**, use
**buzz-agent** as the runtime, **Buzz shared compute** as the LLM provider,
**Default (auto)** as the model, and **This computer** under **Run on**. Shared
compute is an LLM provider; do not select a remote compute backend as the run
location merely because its name mentions mesh.

## 4b. Relay mode: sharing an external server instead of a local model

Relay mode lets this machine advertise an already-running external
OpenAI-compatible server (LM Studio, vLLM, etc.) to the pool instead of
downloading and serving a local GGUF model. Requires the
`buzz-mesh-relay-plugin` sidecar to exist next to the desktop binary — a
placeholder is enough for `cargo check`/`clippy`/tests
(`just desktop-tauri-check` etc. already do this via `_ensure-sidecar-stubs`),
but a real, built binary is required to actually exercise this end to end:

```bash
. ./bin/activate-hermit
cargo build --release -p buzz-mesh-relay-plugin
./scripts/bundle-sidecars.sh   # copies it (and the other sidecars) into desktop/src-tauri/binaries/
just mesh=1 dev
```

1. Open **Settings → Compute**.
2. Under **Share compute**, set **Source** to **Relay to an external
   server**.
3. Set **Server URL** to the target's OpenAI-compatible base URL (e.g.
   `http://127.0.0.1:1234/v1` for a local LM Studio, or its Tailscale
   address). Set **API key** if the server requires one — leave it blank
   for an unauthenticated target, or on any restart after the first
   successful start (the backend persists and reuses it).
4. Turn on **Share this machine**. There is no model field and no download
   step — the relayed server's own models become visible to the pool once
   the plugin's health probe reaches it (~15s).

Verify:

```bash
# The relayed server's models should appear here, not a locally-downloaded one.
curl -sS http://127.0.0.1:9337/v1/models | jq '.data[].id'

# A completion should actually round-trip to the external server.
curl -sS http://127.0.0.1:9337/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model": "<a model id from the /v1/models response above>", "messages": [{"role": "user", "content": "Reply OK"}], "max_tokens": 4}'

# The API key must never appear in the plugin process's own argv — only the
# path to the file it reads the key from.
ps -eo command | grep '[b]uzz-mesh-relay-plugin'
```

If the very first member to enable relay mode in a fresh mesh hangs on
start rather than binding `:9337`/`:3131`, that's the upstream "serve needs a
model" gate — confirm `desktop/src-tauri/Cargo.toml`'s `mesh-llm-*` git
dependencies are pinned to the Buzz fork branch (`buzz-v0.74.0-relay` at the
time of writing), not the plain upstream tag; the fork's one-line patch to
`should_show_serve_config_help` is what fixes this for a plugin-only config.

**Local-privilege-boundary note:** the relay plugin's auth-injecting proxy
binds `127.0.0.1` on an OS-assigned port with no additional access control of
its own — any other local user on this machine can reach it and get
authenticated relay to the external server for as long as the plugin runs.
This is the same trust boundary as any other loopback-bound developer tool on
a single-user machine; it is not currently hardened for a shared/multi-user
host.

## 5. Optional diagnostics

While Buzz is running:

```bash
# The desktop should own both ports.
lsof -nP -iTCP:9337 -iTCP:3131

# The embedded OpenAI-compatible ingress should advertise the model.
curl -sS http://127.0.0.1:9337/v1/models | jq '.data[].id'

# Fizz should resolve through the real managed-agent subprocesses.
ps -eo pid,ppid,command | grep -E '[b]uzz-(desktop|acp|agent)'
```

If Fizz fails, open its runtime details from the Agents screen first. Common
causes are:

- launched with `just dev` instead of `just mesh=1 dev`;
- a stale process owns `9337`/`3131`;
- the model is still downloading or preparing;
- Fizz is not a member of the channel;
- defaults were changed but not saved;
- no current Buzz membership snapshot is available (admission fails closed).

## Security boundary

Buzz publishes member-signed discovery notes through an ordinary relay-supported
NIP-51 event. The note includes a MeshLLM-key signature binding the member to the
advertised MeshLLM node identity, plus a second signature over the exact endpoint
tokens in the note. Current Buzz membership controls which node identities are
admitted. A serving target is selectable only when its endpoint signature is
valid, its invite token decodes as a bounded Iroh endpoint, and every advertised
relay URL matches this machine's locally configured Iroh relay policy.

`BUZZ_MESH_IROH_RELAYS` defaults to Iroh's production relay set. Set it to `0`
for direct QUIC only, or to a comma-separated HTTPS allowlist for custom relays.
Plain HTTP is accepted only for loopback development relays. Remote status notes
cannot expand this local allowlist.

MeshLLM—not the Buzz relay—carries inference over direct QUIC or its encrypted
iroh relays and enforces the owner allowlist. The dependency is pinned to the
post-v0.72.2 admission fix that prevents a non-member with a leaked invite token
from using passive inference streams. MeshLLM v0.73.1 still performs its owner
check during gossip after transport connection; authenticating before any gossip
is an upstream protocol change and is not claimed by the Buzz-side checks above.

Relay mode (§4b) adds one more boundary on top of the above: the API key
authenticating this machine to the relayed external server is never carried
in mesh gossip, in the generated plugin config, or in the sharing config
persisted to disk — it lives only in a single owner-only-permissioned file
that the relay plugin reads via `--api-key-file`, which also doubles as this
feature's durable storage across app restarts. See §4b for the corresponding
loopback-scoped trust boundary that file's reader (the plugin's own auth
proxy) does not further restrict.
