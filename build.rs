// Embeds the migrations (migrations/*.sql, in the order of drizzle's journal) into the crate.
use std::fmt::Write;
use std::path::Path;

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    println!("cargo:rerun-if-changed={}", dir.display());
    let journal: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta").join("_journal.json")).expect("migrations/meta/_journal.json")).unwrap();
    let mut out = String::from("static MIGRATIONS: &[Migration] = &[\n");
    for entry in journal["entries"].as_array().expect("journal entries") {
        let tag = entry["tag"].as_str().expect("tag");
        let when = entry["when"].as_i64().expect("when");
        let file = dir.join(format!("{tag}.sql"));
        assert!(file.exists(), "missing migration {}", file.display());
        writeln!(out, "    Migration {{ tag: {tag:?}, when: {when}, sql: include_str!({:?}) }},", file.display().to_string()).unwrap();
    }
    out.push_str("];\n");
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("migrations.rs"), out).unwrap();
}
