# Binary search (planned)

A byte-level pattern matcher that scans native artifact sections
for a sequence of masked bytes with optional bounded gaps. The
foundation for two distinct workflows:

1. **Data hunting** — find magic numbers, table headers,
   embedded structures, custom encryption keys, etc.
2. **Instruction hunting** — find a code shape ("any
   ADRP+ADD pair", "every `mov w0, #1; ret`"). Initially the
   user writes byte masks directly; a later instruction-pattern
   layer compiles assembly snippets down to byte masks and feeds
   the same engine.

This doc covers the byte-level engine (the first phase) and
sketches how the instruction layer hangs off it later.

## Pattern grammar

Whitespace-separated atoms, parsed left-to-right.

| Atom               | Meaning                                                                |
|--------------------|------------------------------------------------------------------------|
| `c0`               | One byte, value 0xc0 (literal hex; `0x` prefix optional).             |
| `e?`               | One byte, nibble-level wildcard: high nibble = 0xe, low = any.        |
| `?f`               | Symmetric — high any, low = 0xf.                                       |
| `??`               | One byte, any value.                                                   |
| `*`                | Bounded gap: 0..=32 bytes (default).                                   |
| `*(0..16)`         | Bounded gap with explicit max — 0..=16 bytes.                          |
| `*(4..)`           | Min ≥ 4 bytes, max defaults to 32.                                     |
| `*(4..16)`         | Both bounds explicit.                                                  |

All numeric literals are hex without `0x`. Whitespace is
significant only as a separator; tabs and newlines are treated
the same.

### Byte-mask semantics

Each non-gap atom is an `(mask, value)` pair. A candidate byte
`b` matches when `b & mask == value`. So `e?` is `(0xf0, 0xe0)`,
`??` is `(0x00, 0x00)`, and `c0` is `(0xff, 0xc0)`.

### Gap semantics

Greedy left-to-right with backtracking. At a gap atom with
range `min..=max`:

1. Skip `min` bytes.
2. Try matching the next atom; on failure, advance by one and
   retry; on `max+1` failures bail out of this whole match
   attempt at the current start offset.

Gap windows are intentionally bounded — an unbounded `.*`
would balloon match counts on large sections. 32 is the
default because most "find me X near Y" patterns are within
a basic block (~16 instructions × 4 bytes = 64 bytes), but
bounded at 32 keeps backtracking cheap.

### Cross-section boundaries

Matches don't span sections. The engine runs once per text /
data section and reports matches with the section name.

### Endianness

AArch64 instructions are stored little-endian in the file.
Bytes are scanned in file order — the same order the hex view
shows them and the same order the `disasm` verb's `bytes`
field uses. So `mov x0, #0` (encoded `0xd2800000`) is matched
with `00 00 80 d2`, not `d2 80 00 00`. The disasm output's
left-to-right byte column is the canonical form to copy from.

## Examples

A few patterns to ground the syntax. All assume an AArch64
text section unless noted.

### A1. Every `mov w0, #1; ret`

`mov w0, #1` = `0xd2800020` → `20 00 80 d2`
`ret`        = `0xd65f03c0` → `c0 03 5f d6`

```
20 00 80 d2 c0 03 5f d6
```

Two adjacent words, no gap. Useful for finding
returning-true stubs.

### A2. ADRP + ADD pointing into __cstring

ADRP `Xd, page`:    `1 immlo:2 10000 immhi:19 Rd:5`
ADD  `Xd, Xs, #imm`: `1 0010001 sh imm12:12 Rs:5 Rd:5`

We don't care about specific page bits but the opcode-bits
positions in the upper byte are fixed. A loose pattern: any
ADRP into any reg, immediately followed by any ADD.

```
?? ?? ?? 9?    ← ADRP variant byte ends in 0x90/0x9X
?? ?? 4? 91    ← ADD imm12 with bit pattern 0x91XX
```

This is the kind of pattern the *instruction* layer would
generate automatically from `adrp <X>; add <X>, <X>, <*>`,
but a power user can write it by hand today via the byte
engine.

### A3. Function-prologue scan with a gap

A common AArch64 prologue: `stp x29, x30, [sp, #-N]!` followed
within a few instructions by `mov x29, sp`. STP-with-pre-index
encodes the `!` form and tends to start with byte `0xfd`:

```
fd ?? ?? a9 * fd 03 00 91
```

Match: STP word starting `0xa9??????fd`, then up to 32 bytes,
then `mov x29, sp` (`0x910003fd` → `fd 03 00 91`). Catches
prologues regardless of stack frame size.

### A4. Plain data search

Find every occurrence of the bytes `de ad be ef`:

```
de ad be ef
```

No instruction semantics; the same engine handles data
patterns.

## Verb surface

### `bin-search <path> --pattern '<pattern>' [--artifact <ref>] [--section <name>] [--limit <n>]`

Returns JSON of matches. Skill catalog entry + MCP dispatch
included so an LLM can synthesize patterns from a natural-
language description.

```json
{
  "data": {
    "artifact": "abc123…",
    "pattern": "20 00 80 d2 c0 03 5f d6",
    "total": 47,
    "shown": 47,
    "matches": [
      {
        "section": "__text",
        "address": "0x100004c08",
        "length": 8,
        "preview": "mov w0, #1 ; ret"
      },
      …
    ]
  },
  "meta": { "duration_ms": 36 }
}
```

### Preview column

Each match carries a short `preview` string tailored to the
section's kind, so a table renderer can show useful context
without a follow-up call:

- **Text sections** — decode up to two AArch64 instructions
  from the match offset and join them with ` ; `, e.g.
  `mov w0, #1 ; ret`. Undecoded bytes render as `.word
  0xnnnnnnnn`.
- **Data sections** — first 8 bytes of the match as space-
  separated hex, e.g. `de ad be ef 00 12 34 56`.

The CLI's `--text` renderer formats these as a table with
columns `section / address / preview` so the results read at
a glance.

`--section` narrows to one section; otherwise the verb scans
every text + data section in the artifact (excluding bss / debug
/ zero-base sections, same filter the listing builder uses).

`--limit` caps the result count per section. Default unlimited.

## Implementation sketch

### Parser (~50 LOC)

```rust
enum Atom {
    Mask { mask: u8, value: u8 },
    Gap { min: u8, max: u8 },
}

fn parse(s: &str) -> Result<Vec<Atom>> { … }
```

Tokenize on whitespace. For each token:
- Starts with `*` → parse `*` or `*(N..M)` → `Gap { … }`.
- Two hex chars, possibly with `?` for wildcards → compute
  `(mask, value)`.
- `0x` prefix on a 2-char hex token → strip it.

Errors: empty pattern, unknown token, range bounds out of
order, gap max > 256 (defensive cap), more than 1024 atoms
total (defensive cap).

### Matcher (~80 LOC)

```rust
fn matches_at(atoms: &[Atom], bytes: &[u8]) -> Option<usize> {
    fn go(atoms: &[Atom], bytes: &[u8], pos: usize) -> Option<usize> {
        match atoms.split_first() {
            None => Some(pos),
            Some((Atom::Mask { mask, value }, rest)) => {
                if pos < bytes.len() && bytes[pos] & mask == *value {
                    go(rest, bytes, pos + 1)
                } else {
                    None
                }
            }
            Some((Atom::Gap { min, max }, rest)) => {
                let lo = pos + *min as usize;
                let hi = (pos + *max as usize).min(bytes.len());
                (lo..=hi).find_map(|p| go(rest, bytes, p))
            }
        }
    }
    go(atoms, bytes, 0)
}

fn scan(atoms: &[Atom], section: &[u8]) -> Vec<Match> {
    let mut out = Vec::new();
    for start in 0..section.len() {
        if let Some(end) = matches_at(atoms, &section[start..]) {
            out.push(Match { start, length: end });
        }
    }
    out
}
```

Two optimisations worth adding once the simple form works:
- **First-atom literal-byte fast path**: if the first atom is
  `Mask { mask: 0xff, value: V }`, scan for `V` with
  `memchr::memchr` and only attempt full matches there.
- **Overlap policy**: do we report every offset that matches
  or only non-overlapping matches? Default: every offset.
  Could expose `--non-overlapping` later.

### Speed targets

- 24 MB `__text` section, single-byte-anchored pattern with
  no gaps: < 50 ms.
- Same section, pattern with one gap of width 32: < 200 ms.
- Same section, pattern with three gaps: ~ 1 second (worth
  it for the expressiveness; tighten bounds for speed).

These are easily met without indexing for the kind of pattern
the user writes interactively.

## Phasing

| Phase | Scope                                                                 |
|-------|-----------------------------------------------------------------------|
| A     | Byte engine + parser + `bin-search` CLI verb + JSON output.           |
| B     | Skill catalog entry + MCP dispatch.                                   |
| C     | GUI palette mode (Cmd-Shift-F variant) showing scoped match list.     |
| D     | Instruction-pattern layer that compiles `adrp <X>; add <X>, …` etc.   |

Phase A is self-contained and ships independently. Phases B and
C hang off it without needing further engine changes. Phase D
needs a `Capture { name, mask, value }` atom kind so a single
ADRP's destination register can be re-asserted in the ADD —
that's a meaningful extension and warrants its own design
round when we get there.

## Open questions for later

- **Overlap policy** default: every match vs first-non-overlapping.
- **Cross-section linkage**: handy when a pattern legitimately
  straddles `__text` and `__stubs` (PLT thunks); ignored for v1.
- **Case sensitivity** of section filter (probably should be
  case-insensitive — `__TEXT` is sometimes capitalised in
  Mach-O speak).
- **Result navigation**: should a `bin-search` match be
  followable via the same SearchJump infrastructure as the
  cmd-F palette? Probably yes — `SearchJump::Hex` for data
  sections, `SearchJump::Listing` for text. Phase C work.
