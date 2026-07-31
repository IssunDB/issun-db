// `Playground::build_ref` reads `ISSUNDB_BUILD_REF` through `option_env!`, which is resolved at
// compile time. Without this, cargo has no reason to rebuild when the variable changes, so a second
// build in the same tree would keep reporting the first build's commit.
fn main() {
    println!("cargo:rerun-if-env-changed=ISSUNDB_BUILD_REF");
}
