# Keybindings and keymaps

scriptschnell-term ships six keymap presets and lets you override or remove any binding in
`settings.json` or interactively on **Settings > Keyboard** (`Ctrl+,`).

## Default keymap (`cmdterm`)

The default preset uses `Ctrl+Shift` combinations so standard `Ctrl+letter` shortcuts
reach your shell and terminal programs unmodified.

| Keys | Action |
| --- | --- |
| `Ctrl+Shift+P` | Open command palette |
| `Ctrl+,` | Open settings |
| `Ctrl+Shift+S` | Search worktrees in the sidebar |
| `Ctrl+Shift+B` | Toggle sidebar visibility |
| `F6` / `Shift+F6` | Cycle focus between panel, tab strip, status bar and sidebar search (`Escape` returns to the panel) |
| `Ctrl+Shift+T` | New shell in a new tab |
| `Ctrl+Shift+A` | Start the default coding agent in a new tab |
| `Ctrl+Shift+F` | Browse worktree files in a new tab |
| `Ctrl+Shift+H` | Open worktree history in a new tab |
| `Ctrl+Shift+R` | Review worktree changes against its base branch |
| `Ctrl+Shift+W` | Close the current tab |
| `Ctrl+Shift+D` | Split the focused pane side by side |
| `Ctrl+Shift+E` | Split the focused pane stacked |
| `Ctrl+Shift+X` | Close the focused pane |
| `Ctrl+Shift+M` | Move the focused pane to a new tab |
| `Ctrl+Shift+O` / `Ctrl+Shift+I` | Focus next / previous pane |
| `Ctrl+Shift+Alt+Right` / `Left` | Grow / shrink the focused pane |
| `Ctrl+PageDown` / `Ctrl+PageUp` | Switch to next / previous tab |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy selection / paste (in terminals, file and history panels and debug browser) |
| `Shift+PageUp` / `Shift+PageDown`, wheel | Scroll terminal scrollback |

Without an active selection, `Ctrl+Shift+C` does nothing and never reaches the running
program as `Ctrl+C`.

## Mouse and drag-and-drop

| Gesture | Action |
| --- | --- |
| Double-click in terminal | Select a word (paths, URLs and `file.rs:12:3` count as one word); drag to extend by words |
| Triple-click in terminal | Select a wrapped line; drag to extend by lines |
| `Shift+click` in terminal | Extend selection, or bypass mouse grabbing in programs that capture the mouse |
| Drag tab along tab strip | Reorder tabs |
| Drag tab onto a pane edge | Join that tab's panes into the shown tab as a split on that side |
| Drag pane title onto another pane's edge | Move the pane to that side |
| Drag pane title onto another pane's middle | Swap the two panes |
| Drag pane title onto `+` after the tabs | Move the pane into its own tab |
| Drag pane title onto another tab | Move the pane into that tab, beside its focused pane |

## Keymap presets

Switch presets on **Settings > Keyboard** or with `"keymap"` in `settings.json`:

| Preset id | Style |
| --- | --- |
| `cmdterm` | Default `Ctrl+Shift` shortcuts |
| `tmux` | `Ctrl+B` prefix (`Ctrl+B Ctrl+B` sends `Ctrl+B` to the shell) |
| `vscode` | VS Code integrated terminal shortcuts (`Ctrl+Shift+\`` new terminal, `Ctrl+Shift+5` split) |
| `cursor` | VS Code shortcuts plus `Ctrl+Shift+J` for settings |
| `windows-terminal` | `Alt+Shift+Plus` / `Alt+Shift+Minus` splits, `Alt+arrows` pane focus |
| `konsole` | `Ctrl+(` / `Ctrl+)` splits, `Shift+arrows` tab switching |

## Customizing bindings

**Settings > Keyboard** shows every active binding and its source (preset, extension or
user). Click any binding to record a new key sequence (`Escape` cancels).

You can also edit `settings.json` directly. Map a key sequence to an action name, or to
`null` to disable a preset or extension binding:

```json
{
  "keymap": "tmux",
  "keybindings": {
    "ctrl+b |": "split_side_by_side",
    "ctrl+b %": null,
    "ctrl+alt+g": "command:git-helper.log"
  }
}
```

- **Sequences**: up to three chords separated by spaces (e.g. `ctrl+b |`).
- **Modifiers**: `ctrl` (`control`), `shift`, `alt` (`option`), `super` (`meta`, `cmd`, `win`).
- **Named keys**: `enter`, `tab`, `space`, `backspace`, `escape` (`esc`), `delete`,
  `insert`, `home`, `end`, `pageup`, `pagedown`, `left`, `right`, `up`, `down`, `f1` to
  `f12`, `plus`, `minus`, `comma`.
- **Precedence**: your bindings override extensions, and extensions override the preset.

### Available actions

- **Navigation and UI**: `palette`, `settings`, `search_worktrees`, `toggle_sidebar`,
  `focus_tabs`, `focus_back`
- **Panels**: `new_terminal`, `new_agent`, `files`, `explore`, `review`, `monitor`,
  `tasks`, `debug_browser`
- **Tabs and panes**: `close_panel`, `close_pane`, `close_tab`, `move_pane_to_tab`,
  `split_side_by_side`, `split_stacked`, `focus_next_pane`, `focus_previous_pane`,
  `grow_pane`, `shrink_pane`, `next_tab`, `previous_tab`, `tab_1` through `tab_9`,
  `pane_1` through `pane_8`
- **Custom**: `send_text:<text>` (sends text to the focused terminal),
  `command:<extension command>` (runs a command registered by an extension)
