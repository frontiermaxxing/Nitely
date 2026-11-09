//! An app older than the installed nitely lands approved changes with the installed build, not
//! its own older code, and says so once on the board (NIT-752).

use nitely_core::auto_land::{build_of, round_with, AutoLander, Build, Ran};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn start(home: PathBuf) -> impl Fn(&Path) -> Command {
    move |exe| {
        let mut cmd = Command::new(exe);
        cmd.env("NITELY_HOME", &home);
        cmd
    }
}

fn handovers(home: &Path) -> i64 {
    let conn = rusqlite::Connection::open(home.join("nitely.db")).unwrap();
    conn.query_row("SELECT COUNT(*) FROM event WHERE actor = 'nitely:auto-land' AND summary LIKE 'The app is older%'", [], |r| r.get(0)).unwrap()
}

#[test]
fn an_app_older_than_the_installed_build_hands_its_land_rounds_to_it() {
    let home = nitely_core::paths::unique_temp("nitely-auto-land-handover", "");
    std::fs::create_dir_all(&home).unwrap();
    let db = nitely_core::Db::open(&home.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    let installed = PathBuf::from(env!("CARGO_BIN_EXE_nitely"));
    let start = start(home.clone());
    let build = build_of(start(&installed)).expect("the installed build names its commit");

    // The app built from the same commit as the installed nitely lands itself.
    let mut same = AutoLander::new(Some(build.clone()));
    assert_eq!(same.choose(&db, Some(&installed), &start).unwrap(), Ran::InProcess);

    // An app built from an earlier commit hands every round over, and says so once.
    let older = Build { commit: "0".repeat(40), committed_at: build.committed_at - 86_400 };
    let mut lander = AutoLander::new(Some(older.clone()));
    lander.can_hand_over = true;
    for _ in 0..2 {
        assert_eq!(lander.choose(&db, Some(&installed), &start).unwrap(), Ran::Installed(installed.clone()));
    }
    assert_eq!(handovers(&home), 1);

    // Where only the app can check approvals (macOS), it keeps landing itself and says once that
    // it needs a restart.
    let mut app_only = AutoLander::new(Some(older));
    app_only.can_hand_over = false;
    for _ in 0..2 {
        assert_eq!(app_only.choose(&db, Some(&installed), &start).unwrap(), Ran::InProcess);
    }
    assert_eq!(handovers(&home), 2);

    // The round runs in the installed build and gives back what was already noted, so a lasting
    // error still goes on its issue once across rounds.
    let mut reported = HashMap::from([("APP-1".to_string(), "Nitely could not land it after the approval: no repo".to_string())]);
    let landed = round_with(start(&installed), &mut reported).unwrap();
    assert!(landed.is_empty());
    assert_eq!(reported.len(), 1);
    std::fs::remove_dir_all(&home).ok();
}
