mod edit;
mod error;
mod filter;
mod opts;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

use atty::Stream::{Stderr, Stdout};
use prettyprint::{PagingMode, PrettyPrinter};
use quote::quote;
use structopt::StructOpt;

use crate::error::Result;
use crate::opts::{Args, Opts};

fn main() {
    let result = cargo_expand_or_run_nightly();
    process::exit(match result {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(&mut io::stderr(), "{}", err);
            1
        }
    });
}

fn cargo_expand_or_run_nightly() -> Result<i32> {
    const NO_RUN_NIGHTLY: &str = "CARGO_EXPAND_NO_RUN_NIGHTLY";

    let maybe_nightly = !definitely_not_nightly();
    if maybe_nightly || env::var_os(NO_RUN_NIGHTLY).is_some() {
        return cargo_expand();
    }

    let mut nightly = Command::new("cargo");
    nightly.arg("+nightly");
    nightly.arg("expand");

    let mut args = env::args_os().peekable();
    args.next().unwrap(); // cargo
    if args.peek().map_or(false, |arg| arg == "expand") {
        args.next().unwrap(); // expand
    }
    nightly.args(args);

    // Hopefully prevent infinite re-run loop.
    nightly.env(NO_RUN_NIGHTLY, "");

    let status = nightly.status()?;

    Ok(match status.code() {
        Some(code) => code,
        None => {
            if status.success() {
                0
            } else {
                1
            }
        }
    })
}

fn definitely_not_nightly() -> bool {
    let mut cmd = Command::new(cargo_binary());
    cmd.arg("--version");

    let output = match cmd.output() {
        Ok(output) => output,
        Err(_) => return false,
    };

    let version = match String::from_utf8(output.stdout) {
        Ok(version) => version,
        Err(_) => return false,
    };

    version.starts_with("cargo 1") && !version.contains("nightly")
}

fn cargo_binary() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| "cargo".to_owned().into())
}

fn cargo_expand() -> Result<i32> {
    let Opts::Expand(args) = Opts::from_args();

    let rustfmt;
    match (&args.item, args.ugly) {
        (Some(item), true) => {
            eprintln!("ERROR: cannot expand single item ({}) in ugly mode.", item);
            return Ok(1);
        }
        (Some(item), false) => {
            rustfmt = which_rustfmt();
            if rustfmt.is_none() {
                eprintln!(
                    "ERROR: cannot expand single item ({}) without rustfmt.",
                    item
                );
                eprintln!("Install rustfmt by running `rustup component add rustfmt --toolchain nightly`.");
                return Ok(1);
            }
        }
        (None, true) => rustfmt = None,
        (None, false) => rustfmt = which_rustfmt(),
    }

    let mut builder = tempfile::Builder::new();
    builder.prefix("cargo-expand");
    let outdir = builder.tempdir().expect("failed to create tmp file");
    let outfile_path = outdir.path().join("expanded");

    // Run cargo
    let mut cmd = Command::new(cargo_binary());
    apply_args(&mut cmd, &args, &outfile_path);
    let code = filter_err(&mut cmd, ignore_cargo_err)?;

    let mut content = fs::read_to_string(&outfile_path)?;
    if content.is_empty() {
        let _ = writeln!(
            &mut io::stderr(),
            "ERROR: rustc produced no expanded output"
        );
        return Ok(if code == 0 { 1 } else { code });
    }

    // Run rustfmt
    if let Some(rustfmt) = rustfmt {
        // Work around rustfmt not being able to parse paths containing $crate.
        // This placeholder should be the same width as $crate to preserve
        // alignments.
        const DOLLAR_CRATE_PLACEHOLDER: &str = "Ξcrate";
        content = content.replace("$crate", DOLLAR_CRATE_PLACEHOLDER);

        // Discard comments, which are misplaced by the compiler
        if let Ok(mut syntax_tree) = syn::parse_file(&content) {
            edit::remove_macro_rules(&mut syntax_tree);
            if let Some(filter) = args.item {
                filter::filter(&mut syntax_tree, &filter);
                if syntax_tree.items.is_empty() {
                    eprintln!("WARNING: no such item: {}", filter);
                    return Ok(1);
                }
            }
            content = quote!(#syntax_tree).to_string();
        }
        fs::write(&outfile_path, content)?;

        let rustfmt_config_path = outdir.path().join("rustfmt.toml");
        fs::write(rustfmt_config_path, "normalize_doc_attributes = true\n")?;

        // Ignore any errors.
        let _status = Command::new(rustfmt)
            .arg(&outfile_path)
            .stderr(Stdio::null())
            .status();

        content = fs::read_to_string(&outfile_path)?;
        content = content.replace(DOLLAR_CRATE_PLACEHOLDER, "$crate");
    }

    // Run pretty printer
    let do_color = match args.color.as_ref().map(String::as_str) {
        Some("always") => true,
        Some("never") => false,
        None | Some("auto") | Some(_) => atty::is(Stdout),
    };
    let _ = writeln!(&mut io::stderr());
    if do_color {
        if content.ends_with('\n') {
            // Pretty printer seems to print an extra trailing newline.
            content.truncate(content.len() - 1);
        }
        let printer = PrettyPrinter::default()
            .header(false)
            .grid(false)
            .line_numbers(false)
            .language("rust")
            .paging_mode(PagingMode::Never)
            .build()
            .unwrap();

        // Ignore any errors.
        let _ = printer.string(content);
    } else {
        let _ = write!(&mut io::stdout(), "{}", content);
    }

    Ok(0)
}

fn which_rustfmt() -> Option<PathBuf> {
    match env::var_os("RUSTFMT") {
        Some(which) => {
            if which.is_empty() {
                None
            } else {
                Some(PathBuf::from(which))
            }
        }
        None => toolchain_find::find_installed_component("rustfmt"),
    }
}

// Based on https://github.com/rsolomo/cargo-check
fn apply_args(cmd: &mut Command, args: &Args, outfile: &Path) {
    cmd.arg("rustc");
    cmd.arg("--profile=check");

    if let Some(features) = &args.features {
        cmd.arg("--features");
        cmd.arg(features);
    }

    if args.all_features {
        cmd.arg("--all-features");
    }

    if args.no_default_features {
        cmd.arg("--no-default-features");
    }

    if args.lib {
        cmd.arg("--lib");
    }

    if let Some(bin) = &args.bin {
        cmd.arg("--bin");
        cmd.arg(bin);
    }

    if let Some(example) = &args.example {
        cmd.arg("--example");
        cmd.arg(example);
    }

    if let Some(test) = &args.test {
        cmd.arg("--test");
        cmd.arg(test);
    }

    if let Some(bench) = &args.bench {
        cmd.arg("--bench");
        cmd.arg(bench);
    }

    if let Some(target) = &args.target {
        cmd.arg("--target");
        cmd.arg(target);
    }

    if let Some(target_dir) = &args.target_dir {
        cmd.arg("--target-dir");
        cmd.arg(target_dir);
    }

    if let Some(manifest_path) = &args.manifest_path {
        cmd.arg("--manifest-path");
        cmd.arg(manifest_path);
    }

    if let Some(jobs) = args.jobs {
        cmd.arg("--jobs");
        cmd.arg(jobs.to_string());
    }

    cmd.arg("--color");
    if let Some(color) = &args.color {
        cmd.arg(color);
    } else {
        cmd.arg(if atty::is(Stderr) { "always" } else { "never" });
    }

    if args.frozen {
        cmd.arg("--frozen");
    }

    if args.locked {
        cmd.arg("--locked");
    }

    for unstable_flag in &args.unstable_flags {
        cmd.arg("-Z");
        cmd.arg(unstable_flag);
    }

    cmd.arg("--");

    if args.tests && args.test.is_none() {
        cmd.arg("--test");
    }

    cmd.arg("-o");
    cmd.arg(outfile);
    cmd.arg("-Zunstable-options");
    cmd.arg("--pretty=expanded");
}

fn filter_err(cmd: &mut Command, ignore: fn(&str) -> bool) -> io::Result<i32> {
    let mut child = cmd.stderr(Stdio::piped()).spawn()?;
    let mut stderr = io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    while let Ok(n) = stderr.read_line(&mut line) {
        if n == 0 {
            break;
        }
        if !ignore(&line) {
            let _ = write!(&mut io::stderr(), "{}", line);
        }
        line.clear();
    }
    let code = child.wait()?.code().unwrap_or(1);
    Ok(code)
}

fn ignore_cargo_err(line: &str) -> bool {
    if line.trim().is_empty() {
        return true;
    }

    let blacklist = [
        "ignoring specified output filename because multiple outputs were \
         requested",
        "ignoring specified output filename for 'link' output because multiple \
         outputs were requested",
        "ignoring --out-dir flag due to -o flag",
        "ignoring -C extra-filename flag due to -o flag",
        "due to multiple output types requested, the explicitly specified \
         output file name will be adapted for each output type",
    ];
    for s in &blacklist {
        if line.contains(s) {
            return true;
        }
    }

    false
}
