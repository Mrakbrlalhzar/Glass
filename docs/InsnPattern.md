# Instruction patterns (design)

A typed-assembly affordance that compiles user-entered AArch64
instructions down to byte patterns (with optional wildcard
masking). Drives two distinct consumers:

1. **Binary search** — feeds compiled patterns to the
   `bin-search` byte engine in [`BinSearch.md`](BinSearch.md).
   Lets users write `mov w0, #1 ; ret` instead of hand-rolling
   `20 00 80 52 c0 03 5f d6`.
2. **Patching** — same parser, same encoder, but emits a
   concrete 4-byte word (no wildcards) for write-back to a
   binary section. Out of scope for the first cuts; covered in
   the phasing section.

The headline UX is **autocomplete-as-you-type**: the input pane
shows a ranked list of variants whose mnemonic + operand
prefixes still match what the user has typed. The list narrows
as more is entered. Pick a variant with the arrow keys and Tab
to commit it as a template; fill operand slots; Enter to run
the search (or apply the patch).

## Why a separate composer (not just typing into bin-search)

The byte-level engine is already powerful but the syntax is
unforgiving — you have to know that `c0 03 5f d6` is `ret` and
which bit positions the operand fields occupy. The instruction
composer is a thin layer that:

- **Resolves mnemonics**: `mov` is the canonical name even though
  it's an alias of ORR / ADD / MOVZ / MOVN at the encoding level.
- **Validates operand shapes**: `mov w0, w1` and `mov w0, #1` are
  different opcodes; the composer steers you to the right one.
- **Generates correct masks for wildcards**: instead of
  hand-writing `e? ?? ff *` and hoping the bit positions are
  what you think, you type `mov <Wd>, #1` and the composer
  zeros out the Rd field automatically.

It also produces a small enough output (one or two byte-mask
atoms) that the existing bin-search engine handles it without
any changes.

## armv8-encode coverage audit

Audit done against pinned commit (`75a1a765`):

| Component | State | Notes |
|---|---|---|
| Opcode table | ✓ 1157 entries | Full A64 base ISA + NEON / SIMD. |
| `Aarch64Opcode::base_opcode()` + `operands()` | `pub(crate)` | Need `pub` exposure. |
| Per-opcode iterator | ❌ missing | Need `iter_opcodes() -> impl Iterator<&Aarch64Opcode>`. |
| Operand kinds (89 variants) | ✓ ~99% encodable | Only `Nil` (trivial) and `AddrSimm92` (LDR/STR-neg-imm alias) fall through with `Unimplemented`. |
| `encode_instruction(&InstructionTemplate)` | ✓ public | Exactly what Phase 1 needs. |
| `Aarch64Mnemonic::parse(name)` | ✓ public | String → enum, used for mnemonic lookup. |
| Per-operand bit ranges | ❌ not exposed | The encoders know where bits go but each writes them into a `Word`. For wildcards we need `(bit_offset, bit_width)` per operand position. |

**Three small upstream changes needed in `azw413/armv8-encode`:**

1. Add `pub fn iter_opcodes() -> impl Iterator<Item = &'static Aarch64Opcode>`.
2. Promote `Aarch64Opcode::base_opcode()` and `operands()` from
   `pub(crate)` to `pub`.
3. Add `pub fn operand_bit_ranges(&self) -> Vec<Range<u8>>` (or
   `[Option<Range<u8>>; N]`) on `Aarch64Opcode`. For Phase 1
   this isn't needed; Phase 2 wildcards require it.

Phases 1 and 3 can ship without (3); Phase 2 must wait for it.

## Syntax

User input is one or more semicolon-separated instructions,
plus optional gap atoms between them.

```
mov w0, #1 ; ret
adrp <Xd>, <*> ; add <Xd>, <Xd>, <*>
stp x29, x30, [sp, #-0x10]! * mov x29, sp
```

Per instruction:

- Bare mnemonic + comma-separated operands, free-form whitespace.
- Operand slots can be:
  - Concrete: `w0`, `x29`, `#1`, `[sp, #16]`, `0x10000`, etc.
  - Wildcarded: `<*>` matches anything legal for that slot's kind.
  - Slot-kind wildcarded: `<W>` matches any W-class register,
    `<X>` matches any X-class register, `<imm>` matches any
    immediate, etc. (Phase 2 + initially, syntactic; Phase 3
    can also use these as capture names.)
  - Capture (Phase 3): `<Xd:X>` binds `Xd` to the matched register
    and a later `<Xd>` must equal the same register. Captures
    are post-match constraints — the byte-engine produces a
    candidate set, then the composer filters by unification.

Between instructions:

- `;` — adjacent, no gap (next instruction follows immediately).
- `*` — default gap (0..=32 bytes), same semantics as
  [`BinSearch.md`](BinSearch.md).
- `*(min..max)` — explicit gap bounds.

Comments after `#` are line comments where `#` isn't an immediate
prefix (i.e. when not directly preceded by whitespace + register
name). Probably easier to just not support comments in v1.

## Autocomplete UX

Each line in the input has its own autocomplete state. As the
user types, a dropdown shows variants whose mnemonic and
operand-class prefixes still match.

**Example flow** (user types from empty):

| Input | Dropdown (top 5) |
|---|---|
| (empty) | `add`, `adr`, `adrp`, `and`, `asr` |
| `m` | `madd`, `mneg`, `mov`, `mrs`, `msr` |
| `mov` | (operand templates) `mov <Wd>, <Wm>`, `mov <Xd>, <Xm>`, `mov <Wd>, #<imm>`, `mov <Xd>, #<imm>`, `mov <Wd|sp>, <Wn|sp>` |
| `mov w` | narrows to W-class destinations: `mov <Wd>, <Wm>`, `mov <Wd>, #<imm>`, `mov <Wd|sp>, <Wn|sp>` |
| `mov w0, #1` | one variant matches: `mov <Wd>, #<imm>` (fully concrete) |
| `mov w0, #1 ; r` | adds line 2 candidates: `ret`, `rbit`, `rev`, … |

**Selection vs commitment:**

- Up/Down moves selection within the dropdown.
- Tab commits the selected variant — fills in operand
  placeholders that the user hasn't supplied. Cursor lands on
  the next unfilled slot.
- Enter does Tab-then-run: if the line has a fully-committed
  variant with all operands filled (concrete or wildcard), runs
  the bin-search.

**Pre-emptive ambiguity handling:**

When the user has typed enough to be unambiguous (e.g. `mov w0,
#1`), the dropdown shrinks to one row and that row is
preselected. Tab/Enter immediately commits.

When the input is contradictory (e.g. `mov w0, x1` — mixed
register classes), the dropdown shows the variants that match
the *prefix* (the W-target ones) with the ones that match the
*full input* hidden, and an inline error explains the operand-1
mismatch.

## Variants index

Built once at startup from armv8-encode's opcode table. Each row:

```rust
pub struct Variant {
    pub mnemonic: Aarch64Mnemonic,
    /// Display name used in the dropdown ("mov", "mov.cond", "movz").
    pub display: &'static str,
    /// Slot specifications for the dropdown's operand template.
    pub slots: Vec<SlotSpec>,
    /// armv8-encode's base opcode (the bits that are always set
    /// regardless of operand values).
    pub base_word: u32,
    /// Per-slot bit range in the encoded word. Empty for slots
    /// that don't contribute encoded bits (e.g. literal `sp`).
    pub slot_bits: Vec<Range<u8>>,
}

pub enum SlotSpec {
    /// Integer register, GP class (W or X based on context).
    Reg { class: RegClass, optional_sp: bool },
    /// Floating-point or SIMD register.
    Fpreg { class: FpClass },
    /// Vector register with arrangement (V<n>.<arrangement>).
    Vec { …},
    /// Immediate with an encoded width / sign / scaling.
    Imm { width: u8, signed: bool, scale: u8 },
    /// Branch target.
    BranchTarget { width: u8 },
    /// Memory addressing form.
    Mem { … },
    /// Condition code.
    Cond,
    /// System operands (sysreg names, barriers, etc.).
    System { kind: SystemKind },
    /// Literal — operand slot that's not user-editable
    /// (e.g. the optional `lsl #0` on some ALU forms).
    Literal { text: &'static str },
}

#[derive(Copy, Clone)]
pub enum RegClass { W, X }
pub enum FpClass { B, H, S, D, Q }
```

Building the index walks the opcode table, calling each opcode's
`operands()` to get its `Aarch64Opnd` list, and mapping those
into `SlotSpec`s. Estimated 800–1200 visible variants after
deduplicating aliases.

## Parser

State-machine, line-oriented. Each line produces an
`InsnPatternLine`:

```rust
pub enum InsnPatternLine {
    /// One instruction. Variant points into the index;
    /// slot_values is parallel to variant.slots.
    Insn { variant: VariantId, slot_values: Vec<SlotValue> },
    /// Gap between instructions.
    Gap { min: u32, max: u32 },
}

pub enum SlotValue {
    Concrete(DecodedOperand),
    Wildcard,
    Capture { name: String },
}
```

The autocomplete matcher runs the parser incrementally as the
user types. For each candidate variant, it tries to consume the
input up to the cursor; if it succeeds (even partially), the
variant is in the dropdown. Confidence is ranked by how much of
the variant template was consumed.

## Compilation to bytes

For each `Insn` line:

```rust
fn compile(line: &InsnPatternLine, index: &VariantIndex) -> CompiledInsn {
    let variant = &index[line.variant];
    let mut mask = [0xff; 4];
    let mut value_word: u32 = variant.base_word;

    for (slot_idx, slot_value) in line.slot_values.iter().enumerate() {
        let bit_range = &variant.slot_bits[slot_idx];
        match slot_value {
            SlotValue::Concrete(op) => {
                // Use armv8-encode's existing operand encoder.
                let bits = encode_operand(variant, slot_idx, op)?;
                value_word |= bits;
            }
            SlotValue::Wildcard => {
                // Zero out the corresponding mask bits.
                let m = bit_range_mask(bit_range);
                for i in 0..4 {
                    mask[i] &= !((m >> (i * 8)) as u8);
                }
            }
            SlotValue::Capture { .. } => {
                // Same as Wildcard at the byte level; captures
                // are enforced as a post-filter at match time.
                let m = bit_range_mask(bit_range);
                for i in 0..4 {
                    mask[i] &= !((m >> (i * 8)) as u8);
                }
            }
        }
    }

    // Split the 32-bit word into LE bytes.
    let bytes = value_word.to_le_bytes();
    CompiledInsn { mask, value: bytes }
}
```

Multi-line patterns concatenate `CompiledInsn`s as byte-mask
atoms in the bin-search input. Gap atoms between lines pass
through verbatim.

## Captures (Phase 3)

A capture binds an operand-slot value to a name so a later
operand slot can require the same value. Two semantics:

- **Same register**: `adrp <Xd:X>; add <Xd>, <Xd>, <*>` means
  "the Rd of ADRP must equal the Rs and Rd of ADD."
- **Same immediate**: useful for patterns like `mov <Xd>, #<n:imm>;
  cmp <Xd>, #<n>`.

Implementation: the byte-search engine produces candidate sites
based on the masked pattern. For each candidate, the composer
decodes the 4-byte windows and verifies that captured slots
match. Failures drop the candidate.

This is post-filtering, not encoded into the byte mask, because
the byte mask is per-position — it can't express "the same bits
in *this* instruction must equal the corresponding bits in
*that* later instruction." We pay one decode per candidate, which
is cheap (the matcher already touches each match position).

## Output formats

### Bin-search consumer

Each compiled `Insn` produces an 8-character byte-mask token in
the same syntax `bin-search` accepts:

```
mov w0, #1   →   20 00 80 52
ret          →   c0 03 5f d6
mov <Wd>, #1 →   2? 00 80 52
```

(The `?` is the low nibble of byte 0 because Rd occupies the
low 5 bits of the 32-bit word; LE byte 0 = bits 7..0 = Rd[4:0].
A 5-bit wildcard would mask bits 0–4 of byte 0, encoded as
`?` for the low nibble plus a partial-byte mask on the high
nibble. We'd need byte-mask atoms with bit-level granularity
or accept that nibble-level granularity loses some precision.)

**Nibble vs bit granularity for masks** — this is a real design
issue. The current bin-search grammar is nibble-only (`e?`,
`?f`, `??`). AArch64 fields don't align to nibbles. Three
options:

1. **Round masks to nibbles** — some operand bits become
   over-tolerant. A 5-bit Rd wildcard becomes 8-bit (covering
   the low byte) which is fine because the next bits are part
   of the opcode (so a wider mask still rejects non-matching
   instructions). For most operand kinds this works.
2. **Extend bin-search grammar** — add bit-level masks. More
   power but breaks the simple "hex pairs" pattern.
3. **Compile to multiple AND-masked atoms** — a single 32-bit
   `(mask, value)` per instruction, applied as 4 separate
   byte-mask atoms. This is essentially adding a "byte with
   any mask" atom to the grammar.

Option (3) is the right answer; it keeps the byte-level grammar
intact and lets the instruction composer produce maximally-
precise patterns. The grammar gains one new atom form:
`xx/MM` where xx is the value byte and MM is the mask byte
(both 2 hex chars). Existing nibble forms are sugar for
specific mask values.

### Patch consumer (Phase 4)

For patching, the same `Insn` is compiled with **no wildcards
or captures allowed** — must be fully concrete. The output is
a 4-byte word. Apply via a new write-back API:

```rust
glass patch <path> --artifact <ref> --addr 0x... --bytes '<compiled bytes>'
```

The patching design (storage, undo, save-out) is deferred to
its own doc (`docs/Patching.md`).

## Phasing

| Phase | Scope                                                                 | Upstream work                                                                 |
|-------|-----------------------------------------------------------------------|-------------------------------------------------------------------------------|
| A     | Concrete-instruction compiler + CLI verb `insn-search`. No wildcards. | `iter_opcodes()`, `pub` on `base_opcode()` + `operands()`.                    |
| B     | Autocomplete UI in the binary palette tab. Dropdown of variants.      | (none — Phase A APIs cover it.)                                               |
| C     | Operand wildcards `<*>`, slot-kind wildcards `<W>`, `<X>`, `<imm>`.   | `operand_bit_ranges()` on `Aarch64Opcode`; bit-mask atom in bin-search.        |
| D     | Captures `<X>`, cross-line unification.                                | (none — post-filter in glass-api.)                                             |
| E     | Patching: same compiler, concrete-only output, write-back API.         | Patching design separate.                                                      |

Phase A is self-contained and demonstrates the whole pipeline
end-to-end without needing the autocomplete UI yet. The CLI
form lets us iterate on the encoder side first; the dropdown
UX (Phase B) hangs off the same compiler.

## Open questions

- **Mnemonic aliasing**. `mov` is an alias of several encodings;
  the dropdown should show "mov" once and let the user pick
  variants by operand template. The opcode table has multiple
  entries — we'd dedupe by display name, but the alias mapping
  isn't trivial. armv8-encode might have a `display_alias()`
  helper (saw a reference earlier); worth checking what it
  returns.
- **Vector registers**. `add v0.4s, v1.4s, v2.4s` has the
  arrangement specifier inside the operand. The slot specs
  need to capture the arrangement; this might be a separate
  slot or part of the register reference. Probably part of the
  register slot for ergonomic typing.
- **Memory addressing**. `[sp, #16]`, `[sp, #16]!`, `[sp], #16`
  — three distinct forms. The dropdown needs to surface them
  as separate variants of the same parent ld/st mnemonic.
- **Branch targets**. `b 0x10000` or `b <label>` — the composer
  needs to either reject symbolic targets (only addresses) or
  resolve them via the loaded bundle's symbol map. For Phase A
  reject; for the GUI integration we can resolve.
- **Performance**. Building the variants index at startup walks
  ~1157 opcodes; cheap. Running the autocomplete matcher on
  every keystroke is also cheap (linear scan + ranking; 1200
  variants × ~10 atoms each ~= 12k comparisons). No indexing
  needed unless / until profiling says otherwise.

## See also

- [`BinSearch.md`](BinSearch.md) — the byte-level engine this
  layer compiles down to.
- [`cli-api.md`](cli-api.md) — the existing CLI / MCP verb
  reference; `insn-search` will land here once Phase A ships.
