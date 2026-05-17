## Glass Roadmap

* ~~Control flow graph view of disassembly~~
* ~~Control flow graph for DEX methods~~
* ~~CLI command defaulting to GUI (no ‘gui’ command needed)~~
* ~~Closing the last window terminates the command line app~~
* glass menu should be ‘Glass’ *(note: `NSProcessInfo.setProcessName("Glass")` already runs at launch; needs verification on user's machine — may only repro when running the bare `glass` binary from a fresh shell rather than via cargo run)*
* ~~Opening a file in an empty window does not create a new window but reuses this one~~
* Open 2nd file, offset x,y position of 2nd window
* In disassembly view, don’t allow cursor to select basic block rule line
* References search, right click in any view ‘find references’ i.e. calls to current method / function or reference to data location (hex view).
* Goto address function - top bar (straight to section) - validates address as typing (red -> white) and then jump to appropriate view
* Overview bar cursor should go to nearest function start for disassembly views rather than actual address (might not be byte aligned otherwise)
* Package as a MacOS app in a CI job (github action)
* Extend search to search for instruction sequences with operands fuzzy e.g. adrp <reg>; add <reg>, <reg>, <anything>
* script engine API over surface of all functionality
* scripting setup e.g. scripts describe their function and add to menus
* execute scripts
* In place edits (instructions & data), patch and save binary
* Command line capabilities
* MCP skills catalog over API