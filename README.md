<div align="center">

<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://media.x.ai/v1/website/spacexai-symbol-white-transparent-0c31957f.png">
    <source media="(prefers-color-scheme: light)" srcset="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png">
    <img alt="SpaceXAI logo" src="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png" width="96">
  </picture>
  <br>
  Grok Build Max (<code>grokmax</code>)
</h1>

</div>

> **This is a multiprovider Grok Build clone to allow use of multiple model
> providers with failsafe rollover between them.**
>
> Grok Build Max is a fork of [xai-org/grok-build](https://github.com/xai-org/grok-build)
> — SpaceXAI's terminal AI coding agent — extended to talk to Anthropic,
> OpenAI, Gemini, NVIDIA, Ollama, GitHub Copilot, OpenRouter, DeepSeek,
> Cerebras, GLM Coding, and any local OpenAI-compatible endpoint. When the
> active provider dies mid-conversation (quota exhausted, auth failure, fatal
> server error), Grok Build Max silently rolls over to the next provider in
> your ordered failover list, before any output is produced, and tells you it
> did so. API keys live **only** in `config.toml` — never in `.env` files.

<div align="center">

**Grok Build** is SpaceXAI's terminal-based AI coding agent. It runs as a
full-screen TUI that understands your codebase, edits files, executes shell
commands, searches the web, and manages long-running tasks — interactively,
headlessly for scripting/CI, or embedded in editors via the Agent Client
Protocol (ACP).

[Installing](#installing) ·
[Building from source](#building-from-source) ·
[Multi-provider & failover](#what-grok-build-max-adds-multi-provider-support--failover) ·
[Documentation](#documentation) ·
[Repository layout](#repository-layout) ·
[Development](#development) ·
[Contributing](#contributing) ·
[License](#license)

![Grok Build TUI](https://media.x.ai/v1/website/universe-tui-screenshot-6f7a0837.png)

**Learn more about Grok Build at [x.ai/cli](https://x.ai/cli)**

This repository contains the Rust source for the `grok` CLI/TUI and its agent
runtime. It is synced periodically from the SpaceXAI monorepo.

A small `SOURCE_REV` file at the root records the full monorepo commit SHA
for the version of the code present in this tree.

</div>

---

## Installing

One-line installers build Grok Build Max from source and put the `grokmax`
executable on your `PATH` (requires [Rust](https://rustup.rs) and Git):

**macOS / Linux / Git Bash**

```sh
curl -fsSL https://raw.githubusercontent.com/FearL0rd/grok-build-max/main/install.sh | bash
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/FearL0rd/grok-build-max/main/install.ps1 | iex
```

Then verify:

```sh
grokmax --version
```

Both scripts clone this repository to `~/.grokmax/src` (skipped if it already
exists), build the release binary with `cargo build --release -p
xai-grok-pager-bin`, and install it as `grokmax`. The Windows installer also
downloads a protoc release automatically (the repo's `bin/protoc` DotSlash
wrapper has no Windows entry), and requires the
[Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
for linking:

- macOS/Linux: `/usr/local/bin` when writable, else `~/.local/bin` (added to
  `PATH` in your shell profile when needed).
- Windows: `%LOCALAPPDATA%\grokmax\bin` (added to your user `PATH`).

Re-run the same script to update to the latest `main`. To uninstall, delete
the `grokmax` binary and the `~/.grokmax/src` clone.

> The upstream (SpaceXAI) release installers still exist for the original
> Grok Build: `curl -fsSL https://x.ai/cli/install.sh | bash` /
> `irm https://x.ai/cli/install.ps1 | iex` — those install `grok`, not
> `grokmax`.

---

## What Grok Build Max adds (multi-provider support + failover)

### Supported providers

| Preset | `api_backend` | Default base URL | Notes |
|--------|---------------|------------------|-------|
| Grok (built-in) | xAI native | `https://api.x.ai/v1` | Original behavior, unchanged |
| Anthropic | `messages` | `https://api.anthropic.com/v1` | Messages API |
| OpenAI | `chat_completions` | `https://api.openai.com/v1` | |
| Gemini | `gemini` | `https://generativelanguage.googleapis.com` | Native streaming backend |
| NVIDIA NIM | `chat_completions` | `https://integrate.api.nvidia.com/v1` | |
| Ollama (local) | `chat_completions` | `http://localhost:11434/v1` | `keyless = true` |
| GitHub Copilot | `chat_completions` | `https://api.githubcopilot.com` | Dynamic `X-Request-Id` header injected per request |
| OpenRouter | `chat_completions` | `https://openrouter.ai/api/v1` | `HTTP-Referer` header sent automatically |
| DeepSeek | `chat_completions` | `https://api.deepseek.com/v1` | |
| Cerebras | `chat_completions` | `https://api.cerebras.ai/v1` | |
| GLM Coding | `chat_completions` | `https://api.z.ai/api/coding/paas/v4/` | Z.ai GLM coding endpoint |
| Local OpenAI-compatible | `chat_completions` | your URL | llama.cpp, LM Studio, vLLM, `localai`, ... |

### Configuring providers: the `/providers` panel

Inside the TUI, type `/providers` and press Enter. The panel shows:

- **Failover order list** — providers in rollover priority order, top first.
  The currently active model is highlighted.
- **Add** — pick a preset (or a blank OpenAI-compatible entry), enter a name,
  base URL, API key, and model id. Ollama and other keyless endpoints skip
  the key prompt.
- **Remove** — delete a provider entry.
- **Reorder** — move a provider up/down to change failover priority.
- **Rollover notices** — when a provider is skipped or the chain rolls over,
  a notice names the provider that took over (also shown live while the panel
  is open).

Every change is written to `config.toml` immediately and the running session
hot-reloads its failover chain — no restart needed.

### Where settings live: `config.toml` only

All provider settings — **including API keys — live in `config.toml`**
(`~/.grok/config.toml`). Nothing is stored in `.env` files; environment
variables are never consulted for provider keys. The file is yours to edit
by hand too:

```toml
[model.my-openai]
base_url = "https://api.openai.com/v1"
api_key = "sk-..."
api_backend = "chat_completions"
model = "gpt-4o"

[model.my-anthropic]
base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-..."
api_backend = "messages"
model = "claude-sonnet-4-5"

[model.my-ollama]
base_url = "http://localhost:11434/v1"
keyless = true
api_backend = "chat_completions"
model = "qwen2.5-coder:32b"

[failover]
order = ["grok", "my-openai", "my-anthropic", "my-ollama"]
```

- `[model.<name>]` — one section per provider entry. `api_key` is stored in
  plaintext in this file; keep the file's permissions private. Use
  `keyless = true` for endpoints that need no auth. Optional
  `[model.<name>.extra_headers]` adds static headers
  (Copilot's `X-Request-Id` is injected automatically on top of these).
- `[failover]` `order` — the rollover priority list, exactly the order shown
  in the `/providers` panel. Entries are model names; the chain starts at the
  selected model's position.

**Do not commit `config.toml` to version control.**

### How failover works

- The sampler walks the `[failover]` order **forward only** — no wrap-around.
- Rollover triggers on a **fatal error before any output is observed**:
  quota exhausted (429/402), auth rejection (401/403), model not found, or a
  fatal 5xx on the first provider. Once tokens have streamed back, the turn
  is left alone (no mid-answer swaps).
- The chain resets per request: the next user turn starts again from the
  selected model's position.
- Rollovers surface as `ProviderSkipped` / `ProviderRolledOver` /
  `ProviderFailed` events in the TUI (toasts + the `/providers` panel), so
  you always know which provider answered.
- Chain order changes from the panel hot-reload into the live session via
  `x.ai/providers/reload`.

### Building and testing the provider stack

```sh
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/grokmax
cargo test -p xai-grok-sampler               # failover chain walk
cargo test -p xai-grok-shell --lib util::config::providers_io   # config writers
cargo test -p xai-grok-pager --lib providers_modal              # /providers panel
```

## Building from source

Requirements:

- **Rust** — the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml);
  `rustup` installs it automatically on first build.
- **[DotSlash](https://dotslash-cli.com)** — required so hermetic tools under
  [`bin/`](bin/) (notably [`bin/protoc`](bin/protoc)) can download and run.
  Install it and ensure `dotslash` is on your `PATH` **before** building:

  ```sh
  cargo install dotslash
  # or: prebuilt packages — https://dotslash-cli.com/docs/installation/
  /usr/bin/env dotslash --help   # sanity check
  ```

- **protoc** — proto codegen resolves [`bin/protoc`](bin/protoc) via DotSlash,
  or falls back to a `protoc` on `PATH` / `$PROTOC`.
- macOS and Linux are supported build hosts; Windows builds are best-effort
  and not currently tested from this tree.

```sh
cargo run -p xai-grok-pager-bin              # build + launch the TUI
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/grokmax
cargo check -p xai-grok-pager-bin            # fast validation
```

The binary artifact is named `grokmax`. On first launch it opens your browser
to authenticate — see the
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

## Documentation

Full online documentation is available at
[docs.x.ai/build/overview](https://docs.x.ai/build/overview).

The user guide ships with the pager crate:
[`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)
— getting started, keyboard shortcuts, slash commands, configuration, theming,
MCP servers, skills, plugins, hooks, headless mode, sandboxing, and more.

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds the `grokmax` binary |
| `crates/codegen/xai-grok-pager` | The TUI: scrollback, prompt, modals, rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime + leader/stdio/headless entry points |
| `crates/codegen/xai-grok-tools` | Tool implementations (terminal, file edit, search, ...) |
| `crates/codegen/xai-grok-workspace` | Host filesystem, VCS, execution, checkpoints |
| `crates/codegen/...` | The rest of the CLI crate closure (config, MCP, markdown, sandbox, ...) |
| `crates/common/`, `crates/build/`, `prod/mc/` | Small shared leaf crates pulled in by the closure |
| `third_party/` | Vendored upstream source (Mermaid diagram stack) — see below |

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members, dependency versions, lints,
> profiles) is **generated** — treat it as read-only. Prefer editing per-crate
> `Cargo.toml` files.

## Development

```sh
cargo check -p <crate>        # always target specific crates; full-workspace builds are slow
cargo test -p xai-grok-config # per-crate tests
cargo clippy -p <crate>       # lint config: clippy.toml at the repo root
cargo fmt --all               # rustfmt.toml at the repo root
```

## Contributing

> [!NOTE]
> External contributions are not accepted. See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

First-party code in this repository is licensed under the **Apache License,
Version 2.0** — see [`LICENSE`](LICENSE).

Third-party and vendored code remains under its original licenses. See:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and **in-tree source ports** (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts +
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
