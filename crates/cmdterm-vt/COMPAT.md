# Compatibility notes

Where cmdterm-vt differs on purpose from wezterm-term and from xterm. xterm is the reference:
where xterm and wezterm disagree, cmdterm-vt follows xterm and the difference is listed here.
wezterm-term was the oracle of a differential test suite while cmdterm-vt was written. That
suite and the dependency are gone. These notes are what it found, kept so nobody has to
rediscover it. Behaviour is tested in the other files under `tests/`, and the recorded
sessions in `tests/fixtures/` pin the end states.

Each section names the sequence, the behaviour of both emulators and the xterm reference.
The xterm references name the function or behaviour in xterm's source (`charproc.c`,
`screen.c`, `misc.c`) and the xterm control sequences document (ctlseqs). They were
written from memory of that source, not checked against a source tree on this machine.
Treat each one as a pointer to confirm, not as a quotation.

Checked against the source (ThomasDickey/xterm-snapshots, `screen.c`, `util.c`, `ptyx.h`):
the erased-cells-keep-pen entry (`ClearCells`, `makeColorPair`, `TERM_COLOR_FLAGS`) and the
ICH, DCH and IL/DL references that name `ClearCells`. xterm keeps both colors of the pen on
an erased cell, not only the background. The other references are still unverified.

## zero-width-joins-cell-left-of-cursor

- Sequence: a zero-width character (combining mark, ZWJ, variation selector) that starts a chunk of printed text, so the previous character came earlier
- cmdterm-vt: joins the cell left of the cursor, or the cell at the cursor while a wrap waits, whichever way the cursor got there; dropped at column 0 or when it does not join
- wezterm: joins only within one run of printed text; after any other sequence, or a REP, it starts a cell of its own or is dropped
- xterm: xterm WriteText: a combining character is added to the cell before the cursor (the cell at col - 1, back over a wide character's right half), with no memory of how the cursor got there; with the cursor at column 0 it is dropped

## vs16-widens-narrow-base

- Sequence: a narrow character followed by VS16 (U+FE0F), such as a heart or a snowman
- cmdterm-vt: the cluster takes two columns, so the character after it moves right (at the right edge there is no room and the cell stays narrow)
- wezterm: keeps the cluster one column wide
- xterm: xterm treats U+FE0F as a zero-width combining character and keeps the base's wcwidth, so the cluster is one column wide; the widening is a deliberate choice for this crate, as in Unicode TR51 and terminals such as kitty and ghostty

## cluster-reaching-right-margin

- Sequence: a grapheme cluster of several characters whose first character fills the last column (so a wrap waits) and whose later characters join it
- cmdterm-vt: the later characters join the cell and the wrap stays pending until the next printed character
- wezterm: performs the wrap before attaching the later characters: the cursor is on the next line (scrolling at the bottom margin) while they join the cell left behind
- xterm: xterm WriteText: a combining character joins the cell before the cursor and leaves do_wrap set; the wrap happens when the next base character arrives

## vs15-narrows-wide-base

- Sequence: a wide character followed by VS15 (U+FE0E), such as an emoji face
- cmdterm-vt: the cluster becomes one column wide: the second column is blanked and the cursor moves back onto it
- wezterm: keeps the cluster two columns wide
- xterm: xterm treats U+FE0E as a zero-width combining character and keeps the base's wcwidth, so the cluster stays two columns wide; narrowing is a deliberate choice for this crate, as for VS16 widening

## charset-ignored-in-clusters

- Sequence: a grapheme cluster of several characters that starts with an ASCII character, while a national or DEC graphics set is in GL
- cmdterm-vt: maps the first character through the set, then joins the later characters to the mapped cell
- wezterm: maps single characters but prints a multi-character cluster as it is
- xterm: xterm WriteText: the character set maps every character of the run, combining characters included, before it is stored

## wide-char-at-edge

- Sequence: a double-width character printed with one column left on the line, or at the last column with DECAWM off
- cmdterm-vt: with DECAWM on it wraps to the next line first; with DECAWM off it moves the cursor back so the whole character fits and overwrites the cell before it
- wezterm: writes the character at the last column and lets its right half fall outside the screen
- xterm: xterm dotext: a character that does not fit wraps when wrap-around is on; with it off the cursor is placed so the character ends at the right margin

## wide-char-left-of-margin

- Sequence: a double-width character printed while DECLRMM is on and the cursor is left of the left margin
- cmdterm-vt: prints at the cursor and wraps at the right margin, as for any character
- wezterm: reports a cursor column one cell short after the character that reaches the right margin
- xterm: xterm WriteText: only the right margin limits the line; a cursor left of the left margin prints normally

## wrap-in-origin-mode

- Sequence: LF, VT, FF, IND, NEL, RI or a printable character that wraps, while DECOM is on and the top margin is not row 1 (or, with DECLRMM, the left margin is not column 1)
- cmdterm-vt: continues at the left margin of the next line
- wezterm: adds the margin offset a second time: one column too far right, or one line too far down
- xterm: xterm WriteText: after a wrap the cursor goes to the left margin (ScrnLeftMargin), whatever the origin mode

## scroll-in-column-margins

- Sequence: CSI Ps S, CSI Ps T, or a line feed, wrap or reverse index that scrolls, while DECLRMM is on and the margins are narrower than the screen
- cmdterm-vt: moves the cells inside the margins and blanks the vacated lines there with the current background
- wezterm: paints the current background over the margin columns of every line of the region
- xterm: xterm xtermScroll with left/right margins: only the vacated lines are cleared, with the current background

## ich-dch-ignore-background

- Sequence: CSI Ps @ (ICH) or CSI Ps P (DCH) while the current SGR foreground or background is not the default
- cmdterm-vt: fills the blank cells it opens up with the current foreground and background colors
- wezterm: fills them with default colors
- xterm: xterm ScrnInsertChar and ScrnDeleteChar clear the new cells with ClearCells, which stores the current foreground and background colors

## scosc-with-parameters

- Sequence: CSI s and CSI u (and CSI Pn ; Pn s) while DECLRMM is off
- cmdterm-vt: work as DECSC and DECRC: they save and restore the SGR state, charsets and origin mode with the cursor, and CSI s ignores parameters
- wezterm: save and restore the position only, and CSI s with parameters does nothing
- xterm: xterm CASE_ANSI_SC and CASE_ANSI_RC call CursorSave and CursorRestore, the same as DECSC and DECRC; without left/right margin mode CSI s looks at no parameters

## tab-keeps-pending-wrap

- Sequence: HT, CSI Ps I (CHT) or CSI Ps Z (CBT) with a wrap pending
- cmdterm-vt: clears the pending wrap, so the next character overwrites the last column
- wezterm: keeps the pending wrap, so the next character wraps to the next line
- xterm: xterm CursorForward and CursorBack, which CHT and CBT use, call ResetWrap

## il-dl-column-margins-ignore-background

- Sequence: CSI Ps L or CSI Ps M with DECLRMM margins narrower than the screen and a non-default SGR foreground or background
- cmdterm-vt: fills the vacated cells inside the margins with the current foreground and background colors
- wezterm: fills them with the default colors
- xterm: xterm InsertLine and DeleteLine clear the vacated cells with ClearCells, which stores the current foreground and background colors

## cursor-after-wide-char-at-edge

- Sequence: a double-width character that ends at the last column or the right margin
- cmdterm-vt: the cursor stays on the second half of the character, with a wrap pending
- wezterm: reports the cursor on the first half of the character, so relative moves, and text printed over it with wrap-around off, land one column off
- xterm: xterm dotext leaves cur_col on the last column of the line and sets do_wrap

## dec-graphics-underscore

- Sequence: 0x5f printed while a DEC special graphics set is in GL
- cmdterm-vt: prints a blank
- wezterm: prints an underscore
- xterm: xterm's DEC Special Graphics table maps 0x5f to a blank (NBSP)

## tab-ignores-column-margins

- Sequence: HT, CSI Ps I or CSI Ps Z while DECLRMM margins are narrower than the screen
- cmdterm-vt: keeps the cursor between the left and right margins
- wezterm: moves between tab stops across the margins
- xterm: xterm TabToNextStop and TabToPrevStop stop at the margins when left/right margin mode is on

## decstr-clears-pending-wrap

- Sequence: CSI ! p (DECSTR) with a wrap pending
- cmdterm-vt: clears the pending wrap, so the next character overwrites the last column
- wezterm: keeps the pending wrap, so the next character wraps
- xterm: xterm soft reset runs VTReset, which clears do_wrap together with the other cursor state; DECSTR does not move the cursor

## erase-to-end-with-pending-wrap

- Sequence: CSI J or CSI K (mode 0) with a wrap pending
- cmdterm-vt: erases the last column and keeps the wrap pending
- wezterm: treats the cursor as past the last column and erases nothing on the line
- xterm: xterm ClearRight and ED 0 start at cur_col, which is the last column while do_wrap is set

## autowrap-off-keeps-pending-wrap

- Sequence: CSI ? 7 l (DECAWM off) with a wrap pending, then a printable character or CSI ? 7 h
- cmdterm-vt: prints over the last column while wrap-around is off, and wraps if it is turned on again before anything else moves the cursor
- wezterm: wraps to the next line although wrap-around is off
- xterm: xterm WriteText wraps only when both do_wrap and the WRAPAROUND flag are set

## controls-outside-column-margins

- Sequence: CR, LF, VT, FF, IND, NEL, RI, CNL or CPL while DECLRMM is on and the cursor is outside the left and right margins
- cmdterm-vt: treats the cursor as free of the margins: CR goes to column 1 when it is left of the left margin, and vertical moves happen
- wezterm: goes to the left margin for CR, and does not move vertically when the cursor is outside the margins
- xterm: xterm CarriageReturn: a cursor left of the left margin goes to column 1, one at or right of it goes to the left margin

## mode-extra-params

- Sequence: CSI Ps ; Ps J, CSI Ps ; Ps K or CSI Ps ; Ps g
- cmdterm-vt: uses the first parameter
- wezterm: ignores the sequence
- xterm: xterm reads param[0] for ED, EL and TBC and ignores the rest

## locking-shift-2-and-3

- Sequence: ESC n (LS2) and ESC o (LS3)
- cmdterm-vt: invokes G2 or G3 into GL
- wezterm: ignores the sequence, so GL stays G0 or G1
- xterm: ctlseqs: ESC n invokes the G2 character set as GL, ESC o invokes G3

## position-beyond-screen

- Sequence: CSI Pr ; Pc H, CSI Pr ; Pc f and CSI Pr d with a row or column past the screen, and CSI Pc G or CSI Pc ` with a column past it
- cmdterm-vt: clamps to the last row or column
- wezterm: lets the cursor column sit one past the last column
- xterm: xterm CursorSet clamps row and column to the screen (or to the scroll region under DECOM)

## cell-edit-with-pending-wrap

- Sequence: CSI Ps @ (ICH), CSI Ps P (DCH) or CSI Ps X (ECH) with a wrap pending
- cmdterm-vt: edits from the last column; ICH and DCH clear the pending wrap
- wezterm: edits from the last column but keeps the pending wrap
- xterm: xterm InsertChar and DeleteChar call ResetWrap before they edit

## ris-keeps-saved-cursor

- Sequence: ESC c (RIS) followed by ESC 8 (DECRC) or CSI u
- cmdterm-vt: restores the home position with default attributes
- wezterm: restores the cursor saved before the reset
- xterm: xterm ReallyReset clears the saved cursor state of both screens

## vertical-move-keeps-pending-wrap

- Sequence: LF, VT, FF, IND or RI with a wrap pending
- cmdterm-vt: clears the pending wrap, so the next character overwrites the last column
- wezterm: keeps the pending wrap, so the next character wraps
- xterm: xterm Index and RevIndex move the cursor through CursorSet, which calls ResetWrap

## bold-and-faint

- Sequence: SGR 1 and SGR 2 both on
- cmdterm-vt: keeps both attributes
- wezterm: keeps one intensity, the one set last
- xterm: xterm keeps BOLD and FAINT as separate attribute bits; SGR 22 clears both

## wide-char-pushed-over-edge

- Sequence: a double-width character pushed past the right edge by insert mode, ICH or a margin shift
- cmdterm-vt: blanks the half that is left in the last column
- wezterm: keeps the first half in the last column
- xterm: xterm InsertChar and ScrnInsertChar clear a wide character that no longer fits at the right margin

## decstbm-defaults

- Sequence: CSI Pt ; Pb r with a missing, zero or too large parameter
- cmdterm-vt: a missing or zero top is row 1, a missing or zero bottom is the last row, and a bottom past the screen is the last row
- wezterm: mishandles a missing bottom (the sequence has no effect) and ignores a bottom past the screen
- xterm: xterm CASE_DECSTBM: top defaults to 1, bottom to the last row, both clamp to the screen

## margin-beyond-screen

- Sequence: CSI Pl ; Pr s (DECSLRM) with a missing, zero or too large parameter
- cmdterm-vt: a missing or zero left is column 1, a missing or zero right is the last column, and a right margin past the screen is the last column
- wezterm: ignores the sequence when a parameter is zero or the right margin is past the last column
- xterm: xterm CASE_DECSLRM: left defaults to 1, right to the last column, both clamp to the screen width

## ich-dch-outside-region

- Sequence: CSI Ps @ (ICH) or CSI Ps P (DCH) with the cursor above or below the scroll region
- cmdterm-vt: edits the line, because the top and bottom margins do not limit ICH and DCH
- wezterm: ignores the sequence
- xterm: xterm InsertChar and DeleteChar check only the left and right margins

## forward-stops-at-right-margin

- Sequence: CSI Ps C (CUF) or CSI Ps a (HPR) while DECLRMM margins are narrower than the screen
- cmdterm-vt: stops at the right margin
- wezterm: stops at the last column unless origin mode is on
- xterm: xterm CursorForward limits the move to the right margin when left/right margin mode is on

## vertical-move-stops-at-margins

- Sequence: CSI Ps A, B, E, F or e with a scroll region smaller than the screen
- cmdterm-vt: stops at the top or bottom margin when the cursor starts inside the region
- wezterm: stops at the screen edge unless origin mode is on
- xterm: xterm CursorUp and CursorDown limit the move to the scroll region when the cursor starts inside it

## text-right-of-right-margin

- Sequence: a printable character with DECLRMM on and the cursor right of the right margin
- cmdterm-vt: prints on, and wraps at the last column of the screen
- wezterm: wraps as if the right margin applied
- xterm: xterm WriteText uses the right margin as the limit only when the cursor is at or left of it

## decstr-keeps-shift

- Sequence: SO followed by CSI ! p (DECSTR)
- cmdterm-vt: invokes G0 into GL again
- wezterm: resets the character sets but stays shifted out to G1
- xterm: xterm DECSTR resets the character sets and the GL/GR invocation to G0 and G2

## decrc-keeps-shift

- Sequence: SO or SI, then ESC 8 (DECRC) or CSI u
- cmdterm-vt: restores the shift state saved by DECSC
- wezterm: leaves the shift state alone or resets it
- xterm: xterm CursorSave and CursorRestore keep curgl and curgr with the character sets

## insert-delete-inside-wide-char

- Sequence: CSI Ps @ (ICH) or CSI Ps P (DCH) with the cursor on the right half of a double-width character
- cmdterm-vt: cuts the character: both halves become blanks before the cells shift
- wezterm: keeps the character whole and shifts the cells after it
- xterm: xterm InsertChar and DeleteChar blank a wide character that the edit cuts (ScrnWipeWide through the clearing of a split pair)

## insert-mode-with-column-margins

- Sequence: printable text with IRM on and DECLRMM margins narrower than the screen
- cmdterm-vt: shifts the cells between the cursor and the right margin and drops what leaves the margin, cutting a wide character that no longer fits
- wezterm: shifts cells differently around wide characters at the margin
- xterm: xterm WriteText with IRM calls InsertChar, which shifts only up to the right margin

## column-in-origin-mode

- Sequence: CSI Pc G, CSI Pc `, CSI Pr ; Pc H or CSI Pr ; Pc f with DECOM and DECLRMM margins narrower than the screen
- cmdterm-vt: clamps the column to the right margin
- wezterm: allows one column past the right margin
- xterm: xterm CursorSet with origin mode clamps the column to the right margin

## shift-across-wide-char-at-margin

- Sequence: CSI Ps L, M, @ or P with DECLRMM margins narrower than the screen and a double-width character that straddles a margin
- cmdterm-vt: blanks both halves of the character that the margin cuts
- wezterm: moves the half that is inside the margin and keeps the character
- xterm: xterm blanks a wide character that a margin operation splits

## restore-keeps-pending-wrap

- Sequence: ESC 8 (DECRC), CSI u or CSI ? 1049 l that restores a cursor saved with a wrap pending
- cmdterm-vt: restores the pending wrap with the cursor
- wezterm: forgets the pending wrap
- xterm: xterm CursorSave stores do_wrap in wrap_flag and CursorRestore puts it back

## erased-cells-keep-pen

- Sequence: any blank cell made by an erase, insert, delete or scroll (including the scroll of a wrap at the bottom margin) while SGR attributes other than the two colors are set
- cmdterm-vt: an erased cell keeps the foreground and background colors of the pen and none of its attributes
- wezterm: an erased cell keeps the whole pen: colors, underline, strike and so on
- xterm: xterm ClearCells stores the colors from xtermColorPair (cur_foreground and cur_background) and an attribute word of CHARDRAWN plus the color flags only; typed spaces keep the full pen in both emulators and are compared in full

## count-zero

- Sequence: CSI 0 A, B, C, D, E, F, G, `, a, d, e, I, Z, @, P, L, M, S, T or X (an explicit zero count)
- cmdterm-vt: treats 0 as 1
- wezterm: treats 0 as 0 and does nothing
- xterm: ctlseqs: these sequences default to 1, and xterm raises a parameter of 0 to 1

## count-extra-params

- Sequence: the same sequences with more than one parameter, such as CSI 2 ; 5 B
- cmdterm-vt: uses the first parameter
- wezterm: ignores the sequence
- xterm: xterm reads param[0] for these sequences and ignores the rest

## il-dl-cursor-column

- Sequence: CSI Ps L (IL) or CSI Ps M (DL) with the cursor inside the scroll region and margins
- cmdterm-vt: moves the cursor to the left margin
- wezterm: leaves the cursor column alone
- xterm: xterm InsertLine and DeleteLine set the cursor to the left margin, as the VT510 manual says

## rep-wraps-early

- Sequence: a character followed by CSI Ps b (REP)
- cmdterm-vt: prints the character Ps more times like typed text, so the wrap stays pending at the last column
- wezterm: copies the cell left of the cursor and wraps the cursor early
- xterm: ctlseqs: REP repeats the preceding graphic character; xterm prints it through the normal text path

## sgr-empty-parameter

- Sequence: SGR with an empty parameter, such as CSI 1 ; m or CSI ; 31 m
- cmdterm-vt: reads an empty parameter as 0, which resets the attributes
- wezterm: ignores the whole sequence
- xterm: ctlseqs: a missing parameter is 0, and SGR 0 resets

## alt-screen-resets-pen

- Sequence: CSI ? 47 h/l, CSI ? 1047 h/l, CSI ? 1049 h/l
- cmdterm-vt: keeps the current SGR state and the cursor across the switch
- wezterm: resets the SGR state to defaults on every switch, and homes the cursor on 1049 set
- xterm: xterm ToAlternate/FromAlternate swap screen buffers only; the SGR state (xw->flags and colors) is not touched

## decstr-leaves-alt-screen

- Sequence: CSI ! p (DECSTR) while the alternate screen is active
- cmdterm-vt: stays on the alternate screen
- wezterm: switches back to the primary screen
- xterm: xterm VTReset(full = False) for DECSTR does not call FromAlternate; only RIS does

# Resize and reflow

Notes on behaviour that is not a byte sequence. The behaviour is tested in `tests/reflow.rs`.

## reflow-primary-screen

- Sequence: a change of the column count (`Terminal::resize`), not a byte sequence.
- cmdterm-vt: the primary screen and its scrollback reflow. Rows joined by the wrapped flag
  form a logical line that is cut again at the new width. A wide character or cluster that
  does not fit at the end of a row moves whole to the next row, and the row records how many
  columns hold text, so a later reflow does not turn the gap into a space. Trailing default
  blanks are dropped, styled blanks stay. The cursor and the saved cursor stay on their
  character, a cursor that waited to wrap still waits when it sits at the new edge, and
  semantic marks follow their line. Every row gets a new sequence number. The alternate
  screen is cut and padded, never reflowed.
- wezterm: reflows too (`Screen::resize` in `term/src/screen.rs`, from memory). It joins
  rows by its wrapped line flag, tracks the cursor, and keeps a wide character whole.
  Details differ (how many blank rows stay below the cursor, whether the gap left by a wide
  character is text), which is why no case is compared.
- xterm: does not reflow on a column change. It cuts lines at the new width and keeps the
  cursor row (`ScreenResize` in `screen.c`, from memory, unverified). The settled design for
  this crate overrides xterm here on purpose; the rule "xterm wins" applies to byte
  sequences, not to the owner's resize.
  and random resizes keep the logical lines.

## reflow-viewport-placement

- Sequence: the same resize, seen from the viewport.
- cmdterm-vt: the viewport keeps the cursor row at the same distance from the bottom, as
  far as the content allows. Blank rows below the last content and the cursor are not
  content and are added back at the bottom. Content that fits the new viewport starts at
  the top and nothing moves to the scrollback. A saved cursor that ends up in the
  scrollback is clamped to the top row.
- wezterm: keeps the physical rows near the cursor and pushes the rest to the scrollback.
  The exact rule was not checked.
- xterm: keeps the cursor row and pulls lines back from the scrollback when the window
  grows (unverified).

## scrollback-limit-and-marks

- Sequence: scrolling and ED 3 with a full scrollback.
- cmdterm-vt: the limit comes from `Config::scrollback` (0 keeps nothing). Marks of lines
  that left the history are dropped, except the newest of them, which is moved to the oldest
  kept line so that line keeps its zone kind. The style, link and cluster tables are
  collected when a table has grown past what was live and rows have left the scrollback.
- wezterm: keeps its own `StableRowIndex` and drops semantic zones with the lines; its
  style and hyperlink tables are per line and free with the line.
- xterm: has no marks and no tables (unverified). ED 3 clears the saved lines
  (`ClearScrollback`, `screen.c`, from memory).

# Input encoders

cmdterm-vt's `encode_key`, `encode_mouse`, `encode_paste` and `encode_focus` were compared
with wezterm-term's `key_down`, `mouse_event`, `send_paste` and `focus_changed`. The sections
below use the same fields as the screen entries above. The tables in `tests/input.rs` pin
the results.

What the comparison covered: characters, named keys and F1 to F24 with Shift (folded
into the character as the app does), Alt, Ctrl and Super; DECCKM, LNM, modifyOtherKeys
levels 0 to 2; mouse tracking 1000, 1002 and 1003 with the default, 1005, 1006 and 1016
encodings, with positions past the screen edge; bracketed and plain paste; focus reports.

What it left out, and why:

- The kitty keyboard protocol. The pinned wezterm-term parses the flag stack but its
  `KeyCode::encode` has no kitty branch (the GUI encodes kitty keys), so there is nothing to
  compare. `tests/input.rs` holds the tables, including the examples of the protocol text.
- Keypad keys and DECKPAM. wezterm's `KeyCode::encode` leaves a "TODO: also respect
  self.application_keypad" and maps keypad keys as navigation keys.
- Mouse modes 9 and 1015 and the alternate scroll mode 1007. The pinned wezterm has no
  decoding for them. Its wheel code sends cursor keys on the alternate screen without
  looking at mode 1007 and repeats them `alternate_buffer_wheel_scroll_speed` times; the
  reference (xterm `alternateScroll`) is one key per notch with the mode on.
- A release for a button that was not pressed, and a release that does not name
  its button (wezterm ignores it and never clears the held button). A window system does not
  send either.
- A move that names a button. The app sends `MouseButton::None` and the terminal supplies
  the held button; the tables in `tests/input.rs` cover a move with a named button.
- Super with Tab: wezterm drops Shift from the report.
- Focus changes to the state the terminal already has. wezterm ignores them; the owner is
  expected to do the same.
- Large coordinates in the default encoding: past column 222 cmdterm-vt writes nothing (xterm
  does the same), wezterm writes a NUL. The generated positions are clamped to the 80 by 24
  screen, so the case does not arise; `tests/input.rs` covers it with a wide terminal.

As with the screen entries, the xterm references were written from memory of its source and
documentation, not checked against a source tree on this machine.

## super-modifier-dropped

- Sequence: any key pressed with Super held
- cmdterm-vt: Super counts as modifier 8 in the modifier parameter (CSI 1 ; 9 A, CSI 27 ; 9 ; 97 ~), as in the kitty protocol and xterm's meta bit
- wezterm: leaves Super out of the parameter, so the key looks unmodified
- xterm: ctlseqs, PC-style function keys: the modifier parameter is 1 + shift 1 + alt 2 + ctrl 4 + meta 8

## modify-other-keys-2-alt

- Sequence: CSI > 4 ; 2 m, then Alt with a character, Enter, Escape or Backspace
- cmdterm-vt: CSI 27 ; 3 ; code ~, because level 2 reports every modified key
- wezterm: ESC followed by the legacy bytes: only Ctrl triggers the CSI 27 form at level 2
- xterm: ctlseqs XTMODKEYS, modifyOtherKeys 2: send all modified keys, Alt included, as CSI 27 ; modifier ; code ~

## modify-other-keys-1-alt

- Sequence: CSI > 4 ; 1 m, then Alt with Tab, Space or a non-ASCII character
- cmdterm-vt: ESC followed by the key, the same as at level 0
- wezterm: CSI 27 ; 3 ; code ~ for these keys, but ESC plus the key for ASCII letters, digits and punctuation
- xterm: ctlseqs XTMODKEYS, modifyOtherKeys 1: Alt keeps the ESC prefix (metaSendsEscape); only keys with no legacy form change

## modify-other-keys-1-ctrl-alt

- Sequence: CSI > 4 ; 1 m, then Ctrl+Alt with a character that has no control form (a digit, punctuation)
- cmdterm-vt: CSI 27 ; 7 ; code ~, because Ctrl makes the key ambiguous
- wezterm: ESC followed by the character: its Alt rule for ASCII keys runs before the modifyOtherKeys rule
- xterm: ctlseqs XTMODKEYS, modifyOtherKeys 1: a Ctrl-modified key with no control-character form is sent as CSI 27 ; modifier ; code ~

## modify-other-keys-ctrl-uppercase

- Sequence: CSI > 4 ; 1 m, then Ctrl with an uppercase ASCII letter
- cmdterm-vt: the control byte, as for the lowercase letter, with ESC in front for Alt
- wezterm: CSI 27 ; 5 ; code ~ with the uppercase code
- xterm: ctlseqs XTMODKEYS: level 1 leaves keys with a control-character form alone

## modify-other-keys-backspace-ctrl

- Sequence: CSI > 4 ; 2 m, then Ctrl+Backspace
- cmdterm-vt: CSI 27 ; 5 ; 127 ~: Backspace is key code 127, as in the kitty protocol
- wezterm: CSI 27 ; 5 ; 8 ~: the code of the control character it would send without the mode
- xterm: ctlseqs XTMODKEYS names the key by the character it sends (DEL, 127) when backarrowKey is on

## newline-mode-enter

- Sequence: CSI 20 h, then Enter
- cmdterm-vt: CR LF
- wezterm: CR only; the mode is parsed but Enter ignores it
- xterm: ctlseqs LNM: with the mode set, Return sends CR LF (input.c adds the linefeed)

## tab-ctrl-csi-u

- Sequence: Ctrl+Tab without modifyOtherKeys
- cmdterm-vt: HT, as xterm does; the app cannot tell it from Tab
- wezterm: CSI 9 ; 5 u, a CSI u form even though the kitty protocol is off
- xterm: ctlseqs: without modifyOtherKeys, Ctrl+Tab sends HT

## tab-ctrl-shift-param

- Sequence: Ctrl+Shift+Tab
- cmdterm-vt: CSI 1 ; 6 Z with the real modifier parameter (1 + shift + ctrl)
- wezterm: CSI 1 ; 5 Z: the parameter for Ctrl alone
- xterm: ctlseqs: back-tab with modifiers is CSI 1 ; modifier Z

## mouse-release-modifiers-x10

- Sequence: button release with Shift, Alt or Ctrl held in the default or UTF-8 encoding
- cmdterm-vt: button byte 3 plus the modifier bits (4, 8, 16)
- wezterm: button byte 3 without the modifier bits
- xterm: ctlseqs mouse tracking: the modifier bits are added to every button byte, release included

## mouse-motion-dedupe

- Sequence: a pointer move to the cell of the previous reported event (button-event and any-event tracking)
- cmdterm-vt: reports nothing when the cell equals that of the last event it reported, press and release included; reports a move that differs from it
- wezterm: remembers only the last move, so a move after a press in the same cell is reported and a move back to the last move's cell is not
- xterm: ctlseqs and xterm's mouse code (mouse_row, mouse_col) suppress a motion report when the cell did not change since the last report

## focus-lost-releases-buttons

- Sequence: focus lost while a mouse button is held
- cmdterm-vt: only the focus report; the owner decides whether to send releases
- wezterm: first writes a release report for every held button, then the focus report
- xterm: ctlseqs focus reporting: only CSI O is sent; xterm does not synthesize releases

## paste-escape-stripped

- Sequence: bracketed paste of text with ESC
- cmdterm-vt: removes every ESC, so the text cannot start a sequence
- wezterm: removes only the literal CSI 200 ~ and CSI 201 ~ strings
- xterm: xterm's bracketedPasteFilter drops control characters from a paste (disallowedPasteControls picks which); cmdterm-vt strips only ESC, the byte that could end the bracket, and leaves other C0 controls to the program

## paste-c1-csi-stripped

- Sequence: bracketed paste of text with U+009B
- cmdterm-vt: removes it
- wezterm: sends it through
- xterm: xterm's bracketedPasteFilter drops C1 controls from a paste; cmdterm-vt strips only U+009B, the one C1 character that starts a CSI and so could end the bracket, and keeps the other C1 characters

## paste-bracketed-newlines

- Sequence: bracketed paste of text with LF or CR LF
- cmdterm-vt: LF becomes CR and CR LF becomes CR, as in an unbracketed paste
- wezterm: leaves line ends alone inside brackets (NewlineCanon::None)
- xterm: xterm's selection paste turns LF into CR (_WriteSelectionData) whether or not the paste is bracketed

## paste-unbracketed-marker-stripped

- Sequence: paste without bracketed paste mode, text with CSI 200 ~ or CSI 201 ~
- cmdterm-vt: sends the bytes: with no bracket there is nothing to close
- wezterm: removes both marker strings in every mode
- xterm: without mode 2004 xterm sends the selection as typed text and changes only line ends

## decrc-resets-lnm

- Sequence: DECRC (ESC 8, CSI u) while LNM (CSI 20 h) is on
- cmdterm-vt: restores the cursor and leaves LNM on
- wezterm: turns LNM off, so a later line feed no longer returns to column 1
- xterm: xterm CursorRestore restores position, rendition, charsets and origin mode only; LNM is a screen flag it does not touch

## lnm-ignores-left-margin

- Sequence: LF, VT or FF while LNM (CSI 20 h) and DECLRMM are on
- cmdterm-vt: the cursor goes to the left margin of the next line
- wezterm: the cursor goes to column 1 and the next carriage return corrects it only if the program sends one
- xterm: xterm xtermIndex followed by CarriageReturn when LINEFEED is set; CarriageReturn stops at the left margin

## kitty-keys-not-encoded

- Sequence: CSI > flags u with any flag, then any key event
- cmdterm-vt: encodes the key as the kitty keyboard protocol says for those flags (CSI code ; modifiers : event u, text with flag 16)
- wezterm: tracks the flags and answers CSI ? u, but its key encoder has no kitty mode: it writes the xterm bytes and nothing for releases
- xterm: kitty keyboard protocol, sections Progressive enhancement and Disambiguate escape codes: with the flag set, ambiguous keys use CSI u; xterm itself has no such protocol, so the kitty specification is the reference

## keypad-text-keys

- Sequence: a keypad digit, Decimal, Divide, Multiply, Subtract, Add or Separator key
- cmdterm-vt: the character the key types, or SS3 and a letter after DECKPAM (ESC =), as xterm does
- wezterm: treats the digits as NumLock-off navigation keys (CSI 2 ~, CSI F, CSI B, ...), sends nothing for the operator keys, and ignores DECKPAM (a TODO in termwiz)
- xterm: ctlseqs, Application keypad mode: after DECKPAM the keypad sends SS3 p to SS3 y for the digits and SS3 j, k, l, m, n, o for the operators; otherwise the characters

## keypad-begin-ignores-decckm

- Sequence: keypad Begin (5 without NumLock) with DECCKM (CSI ? 1 h)
- cmdterm-vt: SS3 E, like the cursor keys under DECCKM
- wezterm: always CSI E
- xterm: ctlseqs, PC-style function keys: the keypad keys that send cursor-key codes follow the cursor-key mode

## mouse-x10-mode-9

- Sequence: CSI ? 9 h, then a mouse press
- cmdterm-vt: reports the press in the default encoding; X10 mode sends no releases, motion or modifiers
- wezterm: does not know mode 9, so it reports nothing
- xterm: ctlseqs, Mouse tracking: CSI ? 9 h is X10 compatibility mode, button presses only

## mouse-urxvt-1015

- Sequence: CSI ? 1015 h, then a mouse event under tracking
- cmdterm-vt: CSI button+32 ; col ; row M with decimal parameters
- wezterm: does not know mode 1015 and writes the default byte encoding
- xterm: ctlseqs, Mouse tracking: CSI ? 1015 h is the urxvt extended coordinates mode

## osc-empty-zone

- Sequence: OSC 133 `B` (end of prompt) with nothing typed after it yet, or any mark followed by no cell of its kind
- cmdterm-vt: `semantic_zones()` reports the zone from the mark on, up to the end of the screen, so a shell waiting at its prompt has an input zone
- wezterm: derives zones from the semantic type stored in cells (`get_semantic_zones`), so a zone with no cell written in it does not exist until the first character lands
- xterm: no OSC 133; xterm has no semantic zones. The mark semantics follow the FinalTerm and kitty shell integration specifications, where `B` starts the input area at the cursor

# Fixture notes

The recorded sessions in `tests/fixtures/` ended in another state in wezterm-term for these
four, and the difference was accepted with an xterm reference. The wezterm cell dumps are
not kept. The `htop-pid` fixture also differed by the erased-cells-keep-pen entry above:
wezterm kept the attributes of the pen on erased blank cells, cmdterm-vt keeps only the colors.

## fixture-ls-resize

- Sequence: a narrowing resize while `ls` output is on screen
- cmdterm-vt: reflows the wrapped rows, so the text is kept ("s  notes.txt  src" at 30 columns)
- wezterm: loses text in its reflow and shows "README.mdtxt  src" where the text is "...main.rs  notes.txt  src"; the cursor ends one row higher
- xterm: does not reflow on a resize at all, so no text is lost or rewritten

## fixture-python-clusters

- Sequence: a heart with U+FE0F and a Devanagari conjunct (consonants joined by a virama)
- cmdterm-vt: widens the heart to two columns on purpose (entry vs16-widens-narrow-base); gives a grapheme cluster the width of its widest character, so the conjunct takes one column (`src/grapheme.rs`, `Widths`)
- wezterm: keeps the heart one column wide and gives the conjunct two
- xterm: gives U+FE0F no width (a zero-width combining character), so the heart stays one column. For the conjunct it puts each consonant in its own cell and the virama and vowel sign join the cell before them, so it takes more than one column

## fixture-python-unicode

- Sequence: a double-width character printed in the last column with wrap-around on
- cmdterm-vt: wraps to the next line first and shows the character at its start
- wezterm: loses the character (the same edge as the entry wide-char-at-edge)
- xterm: `dotext` wraps a double-width character that does not fit in the last column to the next line first

## fixture-reflow-styled

- Sequence: a resize through narrow, wide and narrow again, then typing
- cmdterm-vt: leaves the cursor on the same row and column, so text typed afterwards starts in column 0
- wezterm: moves the cursor to column 10 of the row after its reflow
- xterm: does not reflow on a resize, and a resize leaves the cursor on the same row and column
