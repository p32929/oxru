# Oxru

A small, fast code editor that runs in your terminal — or in its own window — with **all of a project's terminals living in one place**.

![Oxru — editor with the file tree](https://github.com/user-attachments/assets/456e5e68-6a79-4cdf-bb7f-5141cedf8d93)

---

## Why I built this

Two reasons, honestly:

1. **I wanted every terminal for a project in one place.** Most of my projects spin up a handful of long-running processes — a backend, a bundler, a couple of simulators. That usually means a graveyard of OS terminal windows scattered across my desktop. I wanted one app where they all live as tabs — and where running a `run.sh` that *tries* to open new windows just opens them **inside** instead.
2. **I was tired of how much RAM VS Code eats.** Every single time. So I figured: let me make something simple that does the 20% I actually use and stays light.

That's it. It's not trying to be a full IDE. It's a blank screen, a file picker, tabs, and terminals — done well.

## Why Rust

I just wanted to see how far I could push Rust on a real, interactive, GUI-ish thing — and I genuinely like the language. This project was as much an excuse to write a lot of Rust as it was to scratch the itch above.

---

## Screenshots

**Embedded terminals (tabs + auto-grid):**

![Oxru — terminals](https://github.com/user-attachments/assets/64c622ca-66d1-4304-abea-1d85f34911c1)

**Settings — live font size + theme color:**

![Oxru — settings](https://github.com/user-attachments/assets/c2c16305-9b3f-483f-bb5d-92a96ac17fcc)

**File picker:**

![Oxru — file picker](https://github.com/user-attachments/assets/90da4f79-8f51-48ed-a099-11eacef3c66e)

---

## Install

One line:

```sh
curl -fsSL https://raw.githubusercontent.com/p32929/oxru/master/install.sh | sh
```

This builds Oxru from source with Cargo and puts the `oxru` binary on your `PATH`. You'll need a [Rust toolchain](https://rustup.rs) (`cargo`) — the script tells you if it's missing.

<details>
<summary>Or build it yourself</summary>

```sh
git clone https://github.com/p32929/oxru
cd oxru
cargo install --path .
```

For a lean, terminal-only build with no windowing dependencies:

```sh
cargo install --path . --no-default-features
```
</details>

---

## Usage

Open a project (defaults to the current directory):

```sh
oxru                        # open the current folder in the terminal
oxru ~/code/myapp           # open a specific folder
oxru --gui ~/code/myapp     # open it in a real window (bundled fonts, crisp glyphs)
```

You start on a blank screen with a few hints. Everything is keyboard-driven, and the windowed build also takes the mouse — click a tab to switch, click in the editor to drop the cursor.

### Files

- **`Ctrl+F`** — open the file picker. Start typing to fuzzy-search, or browse the project tree with the arrow keys.
- Inside the picker: **`Enter`** open · **`→ / ←`** expand / collapse a folder · **`Ctrl+N`** new file · **`Ctrl+D`** new folder · **`Ctrl+R`** rename · **`Ctrl+X`** delete · **`Esc`** close.

Open files get line numbers and tree-sitter syntax highlighting (Rust, JS, TS, Python, JSON, Go, C, HTML, CSS, Shell, TOML).

### Editing & tabs

| Shortcut | Action |
|---|---|
| `Ctrl+S` / `Ctrl+Shift+S` | Save · Save **all** |
| `Ctrl+W` / `Ctrl+Shift+W` | Close tab · Close **all** (asks per unsaved file) |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+Shift+,` / `Ctrl+Shift+.` | Move the tab left / right |
| `Ctrl+A` | Select all |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / Cut / Paste (system clipboard) |
| `Shift` + arrows | Extend the selection |

### Terminals

| Shortcut | Action |
|---|---|
| `Alt+T` | Open / hide the terminal panel |
| `Alt+N` | New terminal |
| `Alt+W` | Close the current terminal |
| `Alt+G` | Toggle grid (all at once) vs. tabs |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous terminal |

Terminals are named by their **folder · running command** (e.g. `server · node`, `web · vite`) so you can tell them apart at a glance.

**The neat part:** if a script you run inside a terminal tries to open a *new* OS terminal window, Oxru catches it and opens a new tab **inside** instead. Run your project's `run.sh` that fans out into five windows and you get five tabs — no desktop clutter.

### Settings

- **`Ctrl+,`** — open Settings. Change the **font size** live and pick a **theme color** from a palette of popular colors.
- `↑ / ↓` switch sections · `← / →` change the value · `Esc` / `Enter` to close. Your choices are **saved** and restored next launch.

Prefer a file? Drop a `config.toml` at `~/.config/oxru/config.toml` (global) or `<project>/.oxru/config.toml` (per-project, wins over global). See [`examples/config.toml`](examples/config.toml).

---

## Status

Early and intentionally small — it does the few things I reach for daily and tries to do them well, rather than everything. Issues and ideas welcome.

## License

MIT — see [LICENSE](LICENSE). Do whatever you want with it.
