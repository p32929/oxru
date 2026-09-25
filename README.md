# Oxru

A small, fast code editor that runs in your terminal — or in its own window — with **all of a project's terminals living in one place**.

![Oxru — editor with the file tree](https://github.com/user-attachments/assets/456e5e68-6a79-4cdf-bb7f-5141cedf8d93)

---

## Why I built this

Two reasons, honestly:

1. **I wanted every terminal for a project in one place** — not a graveyard of OS terminal windows scattered across my desktop.
2. **I was tired of how much RAM VS Code eats.** Every single time. So I figured: let me make something simple that does the 20% I actually use and stays light.

That's it. It's not trying to be a full IDE. It's a blank screen, a file picker, tabs, and terminals — done well.

## Why not just use VS Code or Neovim?

Oxru isn't trying to beat either one. It's here for four things they didn't do for me:

- **One window per project, not one per repo.** A project that's a backend + a bundler + a mobile app usually means a `run.sh` that fans out into a pile of OS terminal windows you then have to hunt through. Run the same script inside Oxru and those windows become **tabs inside Oxru** — `Alt+G` puts them all on screen at once, each labelled `folder · command`. Nothing in the script changes.
- **Terminal *and* window, same binary.** VS Code is GUI-only, so over SSH or in a plain shell it's just not an option. Oxru opens as a real window by default and runs inside your terminal with `--term`, with the same keys either way.
- **Light, and you can turn it down further.** No Electron, and the status bar shows live memory and FPS so you can see what you're spending. Settings has a terminal FPS dial (`1 → 60`) that throttles only *unattended* output — a build log scrolling past stops costing CPU, while typing always redraws instantly.
- **Nothing to configure.** Neovim is fast and light too, but getting it to the shape I wanted meant a plugin stack to maintain. Oxru is one binary with defaults I'm happy with. (If Neovim's already your happy place — genuinely, stay there.)

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
oxru                        # welcome screen, in a window
oxru ~/code/myapp           # open a folder in a window (bundled fonts, crisp glyphs)
oxru --term ~/code/myapp    # run inside this terminal instead
```

**Windowed is the default.** `--term` (or `-t`) runs Oxru inside the terminal you launched it from; `--gui` still works if you'd rather be explicit. Over SSH — or on a build compiled with `--no-default-features` — the terminal is used automatically, since there's no window to open.

You start on a blank screen with a few hints. Everything is keyboard-driven, and the windowed build also takes the mouse — click a tab to switch, click in the editor to drop the cursor.

> **On the keys below.** They're written in `Ctrl` form, and `⌘` works anywhere `Ctrl` does. **The same combo does the same thing in the terminal and in the window** — one keymap, no per-mode variants to remember. **`F1`** shows the full list, and `Ctrl+Q` quits.

### Files

| Shortcut | Action |
|---|---|
| `Alt+F` | File picker — type to fuzzy-search, or browse the tree with the arrows |
| `Ctrl+Shift+F` | Search in files — project-wide, powered by ripgrep's engine |
| `Ctrl+O` / `Alt+O` | Open a folder · recent folders |

In the picker: `Enter` open · `→ / ←` expand / collapse · `Tab` / `Shift+Tab` search into / out of a folder · `Ctrl+N` new file · `Alt+Shift+N` new folder · `Ctrl+R` rename · `Ctrl+D` delete · `Alt+R` reveal in Finder · `Alt+H` show / hide `node_modules`, build dirs… · `Esc` close.

Open files get line numbers and tree-sitter syntax highlighting (Rust, JS, TS, Python, JSON, Go, C, HTML, CSS, Shell, TOML).

### Editing & tabs

| Shortcut | Action |
|---|---|
| `Ctrl+S` / `Ctrl+Shift+S` | Save · Save **all** |
| `Ctrl+W` / `Ctrl+Shift+W` | Close tab · Close **all** (asks per unsaved file) |
| `Ctrl+Shift+T` | Reopen the last closed tab |
| `Ctrl+F` / `Ctrl+R` | Find in file (`Enter` / `Shift+Enter` next / previous) · show the replace field (`Tab` switches, `Enter` replaces one, `Shift+Enter` all) |
| `Ctrl+\` | Split / unsplit the view |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+Shift+,` / `Ctrl+Shift+.` | Move the tab left / right |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste (system clipboard) |
| `Ctrl+A` | Select all |
| `Ctrl+D` | Add a caret at the next match (`Ctrl+Alt+↑/↓` above / below, `Esc` collapse) |
| `Alt+Z` | Toggle word wrap (off by default — long lines scroll sideways) |
| `Ctrl+Shift+C` / `Ctrl+Shift+R` | Copy the file's path · path relative to the project |
| `Shift` + any move | Extend the selection |
| `Alt+←/→` · `Ctrl+←/→` · `Ctrl+↑/↓` | By word · line start / end · document start / end |

### Terminals

| Shortcut | Action |
|---|---|
| `Alt+T` | Open / hide the terminal panel |
| `Alt+N` / `Alt+W` | New terminal · close the current one |
| `Alt+G` | Grid (all at once) vs. tabs — click a tile to focus it |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous terminal |
| `Alt+K` | Quick-switch by name (type to filter) |
| `Alt+1`–`Alt+9` | Jump straight to terminal N |
| `Ctrl+Shift+←/→` | Move the terminal left / right |
| `Shift+PgUp/PgDn` (`fn+↑/↓`) | Scroll back through history |
| `Alt+↑/↓` | Copy mode — free cursor, `Shift`+arrows to select, `Enter`/`y` copies |

**Click a link** in terminal output and Oxru asks before opening it in your browser — the full URL is shown, `Enter` opens, `Esc` cancels. Only `http://` and `https://` are offered: terminal output is untrusted, so schemes like `file://` or a custom app handler are never actionable. Dragging across a link still selects text as before.

Terminal output uses a **modern ANSI palette** (VSCode's), because the renderer's built-in one is the original xterm table — red `#800000`, blue `#000080` — which measures 1.1–1.6:1 against a dark background, i.e. all but invisible. Every colour is overridable with `ansi_red`, `ansi_bright_blue`, and so on.

Terminals are named by their **folder · running command** (e.g. `server · node`, `web · vite`) so you can tell them apart at a glance.

**The neat part:** if a script you run inside a terminal tries to open a *new* OS terminal window, Oxru catches it and opens a new tab **inside** instead. Run your project's `run.sh` that fans out into five windows and you get five tabs — no desktop clutter.

It covers every everyday way a script starts one: AppleScript `do script` (via `-e`, a heredoc on stdin, or a `.scpt` file), iTerm's `write text`, JavaScript-for-Automation `doScript()`, and `open -a Terminal` / `open -b com.apple.Terminal` / a double-clicked `.command`. Other terminal emulators (iTerm, kitty, Alacritty, WezTerm, Ghostty, Warp, Hyper) are recognised too. Anything that *isn't* a terminal — `display dialog`, `open -a Simulator`, a URL — runs untouched.

If something still slips through, it's logged: every call the shims decline is written to Oxru's log as `shim let a call through to the real binary`, with the exact command, so it can be identified rather than guessed at. One case can't be intercepted at all — a script calling `/usr/bin/osascript` by absolute path bypasses `PATH`, and nothing short of code injection can catch that.

### Files from Finder

Dropping a file on Oxru, or pasting one you copied in Finder, does what the place you're in expects:

|  | **On a terminal** | **Anywhere else** |
|---|---|---|
| **Drop a file** | types its path at the prompt | opens it as a tab |
| **Paste a file** | types its path at the prompt | inserts the path as text |

Dropping means *"bring this in"*, so over the editor it opens the file. Pasting means *"put this at my cursor"*, so it's always text — and the text of a file is its path. Paths are shell-quoted for the terminal (`My\ Docs/a.txt` stays one argument, and a name containing `;` or `$(…)` can't reach the shell) and left raw everywhere else. Several files at once become space-separated arguments in the terminal, one path per line in the editor. Nothing is ever run — the path is typed, not submitted.

In grid mode the drop goes to the terminal **under the pointer**, not whichever was focused. Anything that isn't editable text — a PNG, a folder, an extensionless binary — says so instead of opening a broken tab.

### To-do list & clipboard

| Shortcut | Action |
|---|---|
| `Alt+D` | Global to-do list |
| `Alt+V` | Clipboard history — this session's copies, newest first |

The to-do list is a plain markdown file at `~/.config/oxru/todos.md`, and the dialog is a view over it. Type to add · `Ctrl+V` pastes a list as one task per line · `Space` toggles · `Ctrl+D` deletes · `Ctrl+Shift+D` clears everything completed · **`Ctrl+E` opens the file as a normal editor tab**, so "edit as plain text" is the real editor — undo, multi-cursor, find and all.

Parsing is forgiving and writing normalises: `- [x] a`, `* [X] a`, `[x] a` all mean done, and a **plain line or a `-`/`1.` list item becomes an unchecked task**, so you can paste a list and it turns into checkboxes. Headings and blank lines round-trip untouched, so structure you add yourself survives.

Clipboard history is **in memory only and never written to disk** — a clipboard history is exactly where a password ends up. It holds the last 50 copies from inside Oxru (editor, terminal selection, copy-path); `Enter` pastes into wherever you were — that's all it does, so your system clipboard and the history's order are both left alone. `Ctrl+D` clears it.

`Space` ticks entries to **paste several at once**, newline-separated, in the order they're listed — useful for collecting a few snippets and dropping them in as a block. With nothing ticked, `Enter` still just pastes the highlighted row.

### Settings

- **`Ctrl+,`** — font size, terminal FPS, dialog size, word wrap, and the accent color. Changes apply live; the rest of the UI (status bar, selection) re-tints to match whatever accent you pick.
- `↑ / ↓` switch sections · `← / →` change the value · `Esc` / `Enter` to close. Your choices are **saved** and restored next launch.

The editor draws a **current-line highlight** and **indent guides** (the indent width is detected per file, so a 2-space file gets guides every 2 columns). Both are on by default; turn either off with `[editor] current_line = false` / `indent_guides = false`.

Prefer a file? Drop a `config.toml` at `~/.config/oxru/config.toml` (global) or `<project>/.oxru/config.toml` (per-project, wins over global). See [`examples/config.toml`](examples/config.toml).

---

## Status

Still under active development, but stable enough to daily-drive — it's what I use for pretty much everything now, and I keep updating it whenever I feel like it, so expect things to move. Intentionally small: the handful of things I reach for every day, done well, rather than everything. Issues and ideas welcome.

## License

MIT — see [LICENSE](LICENSE). Do whatever you want with it.

## Contributing

Contributions are warmly welcomed and greatly appreciated! Whether it's a bug fix, new feature, or improvement, your input helps make this project better for everyone.

Before submitting a pull request, please:

1. Create an issue describing the feature or bug fix you'd like to work on
2. Wait for discussion and approval to ensure alignment with project goals
3. Fork the repository and create your feature branch
4. Submit your pull request with a clear description of changes

This approach helps avoid duplicate efforts and ensures smooth collaboration. Thank you for considering contributing!

## Share

Sharing this repository with your friends is just one click away from here

[![facebook](https://user-images.githubusercontent.com/6418354/179013321-ac1d1452-0689-493f-9066-940cf2302b6e.png)](https://www.facebook.com/sharer/sharer.php?u=https://github.com/p32929/oxru/)
[![twitter](https://user-images.githubusercontent.com/6418354/179013351-7d8d6d1c-4ce2-46ab-bef8-4c4765a1b888.png)](https://twitter.com/intent/tweet?url=https://github.com/p32929/oxru/)
[![tumblr](https://user-images.githubusercontent.com/6418354/179013343-3111f55a-3b90-40c7-8487-9777348672b0.png)](https://www.tumblr.com/share?v=3&u=https://github.com/p32929/oxru/)
[![pocket](https://user-images.githubusercontent.com/6418354/179013334-b095c45f-becf-49f4-9ee1-5a731a9b1f85.png)](https://getpocket.com/save?url=https://github.com/p32929/oxru/)
[![pinterest](https://user-images.githubusercontent.com/6418354/179013331-44cd9206-11b1-4b65-becb-5863b61c828f.png)](https://pinterest.com/pin/create/button/?url=https://github.com/p32929/oxru/)
[![reddit](https://user-images.githubusercontent.com/6418354/179013338-7416ae3f-73ba-4522-86e1-1374d7082d22.png)](https://www.reddit.com/submit?url=https://github.com/p32929/oxru/)
[![linkedin](https://user-images.githubusercontent.com/6418354/179013327-ca7b7102-1da8-4b1c-858f-1a6e5f21bd70.png)](https://www.linkedin.com/shareArticle?mini=true&url=https://github.com/p32929/oxru/)
[![whatsapp](https://user-images.githubusercontent.com/6418354/179013353-f477fa0b-3e6f-4138-a357-c9991b23ff88.png)](https://api.whatsapp.com/send?text=https://github.com/p32929/oxru/)

---

## Support

If this saved you time, you can buy me a coffee — it keeps these projects maintained and free.

[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20me%20a%20coffee-%E2%98%95-FFDD00?style=for-the-badge&logo=buymeacoffee&logoColor=black)](https://www.buymeacoffee.com/p32929)

<!-- hire-block -->

---

## 💼 Using this at a company?

I do fixed-price delivery work on my own projects. One invoice, one date, no hourly billing:

| | |
|---|---|
| **White-label build** — this project rebranded, extended and deployed as yours | **$6,500** · 3 weeks |
| **Custom app from scratch** on my own stack, signed and auto-updating | **$12,500** · 6 weeks |
| **Production-hardening sprint** — 72 hours on this project, for your load and your security review | **$999** |
| **Ongoing capacity** — one project-week of my time reserved every month | **$9,000 / month** |

Full details → **[p32929.github.io/hire](https://p32929.github.io/hire/)** · Email **[fayazdevinbox@uberip.com](mailto:fayazdevinbox@uberip.com)** — scoping and quotes are free and I answer within one business day.
