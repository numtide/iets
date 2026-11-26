//! Executable benchmarks measure the CLI and retain its production allocator.
//! `nix run .#bench-callgrind -- --help` lists the runner's options.
use gungraun::{
    BinaryBenchmarkConfig, Callgrind, Command, EventKind, FlamegraphConfig, FlamegraphKind,
    Sandbox, Stdio, binary_benchmark, binary_benchmark_group, main,
};
use std::ffi::OsStr;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::{env, fs};

fn command() -> Command {
    let binary = env::var_os("IETS_BENCH_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/release/iets")
        });
    Command::new(binary)
        .env("IETS_PROGRESS", "false")
        .env("NIX_CONF_DIR", "/nonexistent")
        .env("NIX_USER_CONF_FILES", "")
        .build()
}

#[binary_benchmark]
#[bench::calls(args = ("calls.nix"), teardown = check_output(15_792))]
#[bench::strings(args = ("strings.nix"), teardown = check_output(50_000))]
#[bench::attrs(args = ("attrs.nix"), teardown = check_output(2_003))]
#[bench::derivations(args = ("derivations.nix"), teardown = check_derivations())]
#[bench::producer_inputs(args = ("producer-inputs.nix"), teardown = check_output("c7a9060d2092cd2ca7ce5d5d303dd3e1f4bd4ac4d249496771bc0fc27a3f4ec9"))]
#[bench::fixed_derivations(args = ("fixed-derivations.nix"), teardown = check_output("54e5be761db238fac542a7f7bd5d635b4d10f0989eb246e18754031c7c9e5dcc"))]
fn evaluate(fixture: &str) -> Command {
    command()
        .args(["eval", "--strict", "--json", "--no-cache", "--workers", "1"])
        .arg(workloads().join("synthetic").join(fixture))
        .stdout(Stdio::File("output".into()))
        .build()
}

fn workloads() -> PathBuf {
    env::var_os("IETS_BENCH_WORKLOADS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workloads"))
}

#[binary_benchmark]
#[bench::hello(args = ("nixpkgs-hello.nix"), teardown = check_reference(workloads().join("nixpkgs-hello.nix"), true))]
#[bench::initial_ramdisk(args = ("nixos-release.nix"), teardown = check_reference(workloads().join("nixos-release.nix"), true))]
fn job(fixture: &str) -> Command {
    command()
        .args([
            "eval",
            "--impure",
            "--strict",
            "--json",
            "--no-cache",
            "--workers",
            "1",
        ])
        .env(
            "NIX_PATH",
            env::var_os("NIX_PATH").expect("set NIX_PATH to include nixpkgs"),
        )
        .arg(workloads().join(fixture))
        .stdout(Stdio::File("output".into()))
        .build()
}

// Setup executes outside the measured executable, in a fresh writable sandbox.
// Identical literal input separates parsing/AST rendering and value rendering
// from the genList, arithmetic, concatenation and lookup scaffolding in evaluation.
fn setup_components() {
    let mut source = String::from("let value = {\n");
    for n in 0..2_000 {
        writeln!(
            source,
            "key{n} = {{ label = \"item-{n}\"; values = [ {n} {} {} ]; }};",
            n + 1,
            n + 2
        )
        .unwrap();
    }
    source.push_str("}; in assert builtins.length (builtins.attrNames value) == 2000; value\n");
    fs::write("component.nix", source).unwrap();
}

#[binary_benchmark(setup = setup_components())]
#[bench::parse(args = ("-q"), teardown = check_parse())]
#[bench::ast_json(args = ("--json"), teardown = check_ast())]
fn frontend(format: &str) -> Command {
    command()
        .args(["parse", format, "component.nix"])
        .stdout(Stdio::File("output".into()))
        .stderr(Stdio::File("errors".into()))
        .build()
}

#[binary_benchmark(setup = setup_components())]
#[bench::json(args = ("--json"), teardown = check_reference("component.nix", true))]
#[bench::nix(args = ("--strict"), teardown = check_reference("component.nix", false))]
fn render(format: &str) -> Command {
    command()
        .args([
            "eval",
            "--no-cache",
            "--workers",
            "1",
            format,
            "component.nix",
        ])
        .stdout(Stdio::File("output".into()))
        .build()
}

// Plain nix eval visits the derivation tag of every set it prints (#620).
const SETS: &str = "builtins.genList (i: { n = i; }) 8192";

#[binary_benchmark(teardown = check_plain_sets())]
fn plain_sets() -> Command {
    command()
        .env("IETS_NIX_FALLBACK", "never")
        .env("IETS_CACHE", "false")
        .env("IETS_WORKERS", "1")
        .args(["nix", "eval", "--expr", SETS])
        .stdout(Stdio::File("output".into()))
        .build()
}

fn check_plain_sets() {
    let mut nix = ProcessCommand::new("nix");
    nix.args([
        "--extra-experimental-features",
        "nix-command",
        "eval",
        "--expr",
        SETS,
    ]);
    compare_reference(nix, false);
}

// Teardown validates output after every tool run, outside the measurement.
fn check_output(expected: impl Into<serde_json::Value>) {
    let expected = expected.into();
    let actual: serde_json::Value = serde_json::from_slice(&fs::read("output").unwrap()).unwrap();
    assert_eq!(actual, expected);
}

fn check_derivations() {
    check_output(serde_json::json!({
        "count": 100,
        "digest": "93992c8668eb390e01499c1ea64d40863403ba29ceb9ffb4b544a3c9976e79f9"
    }));
}

fn check_parse() {
    let errors = fs::read_to_string("errors").unwrap();
    assert!(errors.starts_with("1 files, ") && errors.contains(", 0 errors, "));
    assert!(fs::read("output").unwrap().is_empty());
}

fn check_ast() {
    let mut ast: serde_json::Value = serde_json::from_slice(&fs::read("output").unwrap()).unwrap();
    ast.sort_all_objects();
    // Snapshot the fixture's full tree, including source spans. To refresh after
    // an intentional fixture or schema change, inspect a known-good parse --json
    // of component.nix and take FNV-1a of its compact, key-sorted JSON (no LF).
    let hash = serde_json::to_vec(&ast)
        .unwrap()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    assert_eq!(
        hash, 0x2229_1a7b_f5ec_e9a7,
        "component AST snapshot changed"
    );
}

fn check_reference(file: impl AsRef<OsStr>, json: bool) {
    let mut nix = ProcessCommand::new("nix-instantiate");
    nix.args(["--eval", "--strict"]);
    if json {
        nix.arg("--json");
    }
    nix.arg(file);
    compare_reference(nix, json);
}

fn compare_reference(mut nix: ProcessCommand, json: bool) {
    let expected = nix.output().unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    if json {
        check_output(serde_json::from_slice::<serde_json::Value>(&expected.stdout).unwrap());
    } else {
        assert_eq!(fs::read("output").unwrap(), expected.stdout);
    }
}

binary_benchmark_group!(name = evaluation; benchmarks = evaluate);
binary_benchmark_group!(name = components; benchmarks = frontend, render, plain_sets);
fn config() -> BinaryBenchmarkConfig {
    let mut config = BinaryBenchmarkConfig::default();
    config.sandbox(Sandbox::new(true));
    // Expanding recursive call graphs into differential SVGs can consume a lot
    // of memory. Counts and raw profiles are always collected; SVGs are opt-in.
    if env::var("IETS_BENCH_FLAMEGRAPHS").as_deref() == Ok("1") {
        config.tool(
            Callgrind::default().flamegraph(
                FlamegraphConfig::default()
                    .kind(FlamegraphKind::All)
                    .event_kinds([EventKind::Ir, EventKind::TotalRW]),
            ),
        );
    }
    config
}

binary_benchmark_group!(name = nixpkgs; benchmarks = job);
main!(config = config(); binary_benchmark_groups = nixpkgs, evaluation, components);
