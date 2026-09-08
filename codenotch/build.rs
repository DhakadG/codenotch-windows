use std::process::Command;

fn main() {
    tauri_build::build();

    // A build identity that cannot drift from the binary it is compiled into.
    //
    // The build tag used to be a hand-edited constant, which is worth exactly as much as
    // whoever last remembered to bump it. When a running copy could not be matched to a
    // commit, an installer that had silently failed to replace the executable looked
    // identical to one that had worked - and the wrong build was debugged for an hour.
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "nogit".into());

    // Whether the tree had uncommitted changes when this was built. A local build with
    // edits in it is not the commit it names, and saying so avoids chasing a difference
    // that only exists on one machine.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!(
        "cargo:rustc-env=CODENOTCH_BUILD={}{}",
        sha,
        if dirty { "-dirty" } else { "" }
    );
    println!("cargo:rustc-env=CODENOTCH_BUILT_AT={stamp}");

    // Rebuild when HEAD moves, so the stamped commit stays true after a checkout.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
}
