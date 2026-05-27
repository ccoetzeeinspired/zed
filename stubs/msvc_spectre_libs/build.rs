// No-op: skip adding the Spectre-mitigated CRT link search path so the build
// does not require the VS "Spectre-mitigated libs" component. The linker falls
// back to the normal CRT, which is fine for a personal build.
fn main() {}
