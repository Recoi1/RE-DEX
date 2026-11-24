//! `xr-progress` turns progress-build.json + progress-run.json into docs/progress/{progress.json,badge.svg}.
use std::process::Command;

#[test]
fn badge_shows_percentage_and_milestone() {
    let dir = std::env::temp_dir().join(format!("xr_progress_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("progress-build.json"),
        r#"{"build":"t","functions_listed":90,"functions_added_from_branch_targets":10,"functions_generated":100,"undecodable":[{"addr":"0x1","reason":"x"}],"dll_built":true,"build_seconds":1.0}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("progress-run.json"),
        r#"{"exit_code":4,"milestone_reached":"services","interpreted_entries":[{"addr":"0x00100008","mode":"arm","count":3}],"svcs":[],"services":[]}"#,
    )
    .unwrap();
    let out = dir.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_xr-progress"))
        .arg("--build").arg(dir.join("progress-build.json"))
        .arg("--run").arg(dir.join("progress-run.json"))
        .arg("--out").arg(&out)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let svg = std::fs::read_to_string(out.join("badge.svg")).unwrap();
    assert!(svg.starts_with("<svg"), "{svg}");
    assert!(svg.contains("99.00%") && svg.contains("services"), "{svg}");
    let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("progress.json")).unwrap()).unwrap();
    assert_eq!(j["milestone_reached"], "services");
    assert_eq!(j["exit_code"], 4);
    assert_eq!(j["interpreted_entries"][0]["count"], 3);
}
