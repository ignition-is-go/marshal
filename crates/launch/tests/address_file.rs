//! A daemon-address file that names another daemon is refused before launch.

use marshal_launch::address::{check_daemon_address_file, config_dirs_from};

#[test]
fn a_daemon_address_file_naming_another_daemon_is_refused() {
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("addr-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let xdg = root.join("xdg");
    let home = root.join("home");
    std::fs::create_dir_all(xdg.join("marshal")).unwrap();
    std::fs::create_dir_all(home.join(".config/marshal")).unwrap();
    let dirs = config_dirs_from(Some(xdg.clone()), Some(home.clone()));

    // No file anywhere: nothing to override the launch address.
    check_daemon_address_file(&dirs, "ws://marshal.mesh:6155").unwrap();

    // ~/.config's file names another daemon.
    std::fs::write(
        home.join(".config/marshal/daemon-address"),
        "ws://localhost:6155\n",
    )
    .unwrap();
    let err = check_daemon_address_file(&dirs, "ws://marshal.mesh:6155")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ws://localhost:6155") && err.contains("Nothing was started"),
        "{err}"
    );

    // The same address, or an XDG file that matches and so is read first, is fine.
    check_daemon_address_file(&dirs, "ws://localhost:6155/").unwrap();
    std::fs::write(xdg.join("marshal/daemon-address"), "ws://marshal.mesh:6155").unwrap();
    check_daemon_address_file(&dirs, "ws://marshal.mesh:6155").unwrap();

    // An empty first file is skipped, as the shim skips it.
    std::fs::write(xdg.join("marshal/daemon-address"), "\n").unwrap();
    assert!(check_daemon_address_file(&dirs, "ws://marshal.mesh:6155").is_err());
    std::fs::remove_dir_all(&root).unwrap();
}
