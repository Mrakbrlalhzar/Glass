# Glass — project notes

## Module size

Any Rust module over **1500 lines** should be refactored and split into
smaller modules. `glass-ui/src/lib.rs` has historically violated this
and is being progressively modularised; new code should not regrow it.
