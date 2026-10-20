use std::{path::PathBuf, process::{Child, Command, Stdio}, time::{Duration, Instant}};

struct Preview { home: PathBuf, child: Child }
impl Preview {
    fn start(mode: &str, seconds: &str) -> Self {
        let home = nitely_core::paths::unique_temp("nitely-preview-test", "");
        std::fs::create_dir_all(&home).unwrap();
        let script = home.join("browser.cjs");
        std::fs::write(&script, r#"
const fs=require('fs'), cp=require('child_process');
const child=cp.spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});
fs.writeFileSync(process.env.PREVIEW_MARKER, JSON.stringify({pid:process.pid, child:child.pid}));
if(process.env.PREVIEW_MODE==='wait') setInterval(()=>{},1000);
else setTimeout(()=>process.exit(process.env.PREVIEW_MODE==='fail'?7:0),200);
"#).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_nitely"))
            .args(["preview", "start", "http://localhost:12345", "--browser", "node", "--seconds", seconds])
            .arg(format!("--browser-arg={}", script.display())).env("NITELY_HOME", &home)
            .env("PREVIEW_MARKER", home.join("marker.json")).env("PREVIEW_MODE", mode)
            .current_dir(&home).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        Self { home, child }
    }
    fn pids(&mut self) -> (u32,u32) {
        let until = Instant::now()+Duration::from_secs(10);
        loop {
            if let Ok(text)=std::fs::read(self.home.join("marker.json")) {
                if let Ok(value)=serde_json::from_slice::<serde_json::Value>(&text) {
                    return (value["pid"].as_u64().unwrap() as u32,value["child"].as_u64().unwrap() as u32);
                }
            }
            assert!(Instant::now()<until,"fake browser did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn ended(&mut self, success: bool) {
        let until=Instant::now()+Duration::from_secs(10);
        loop {
            if let Some(status)=self.child.try_wait().unwrap() { assert_eq!(status.success(),success);break; }
            assert!(Instant::now()<until,"launcher did not end");std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Preview {
    fn drop(&mut self) { let _=self.child.kill();let _=self.child.wait();let _=std::fs::remove_dir_all(&self.home); }
}
fn gone(pid:u32) {
    let until=Instant::now()+Duration::from_secs(5);
    while nitely_core::procs::alive(pid) { assert!(Instant::now()<until,"owned process {pid} survived");std::thread::sleep(Duration::from_millis(20)); }
}
#[test]
fn preview_normal_failure_and_timeout_end_owned_descendants() {
    for (mode,seconds,ok) in [("ok","10",true),("fail","10",false),("wait","1",false)] {
        let mut preview=Preview::start(mode,seconds);let (root,child)=preview.pids();preview.ended(ok);
        gone(root);gone(child);
        for entry in std::fs::read_dir(preview.home.join("previews")).unwrap() {
            assert!(!entry.unwrap().path().join("profile").exists());
        }
    }
}
#[cfg(windows)]
#[test]
fn preview_forced_launcher_exit_ends_its_job_and_cleanup_refuses_active_session() {
    let mut preview=Preview::start("wait","30");let (root,child)=preview.pids();
    let entry=std::fs::read_dir(preview.home.join("previews")).unwrap().next().unwrap().unwrap();
    let session=entry.file_name();
    let clean=|| Command::new(env!("CARGO_BIN_EXE_nitely")).args(["preview","cleanup"]).arg(&session)
        .env("NITELY_HOME",&preview.home).current_dir(&preview.home).output().unwrap();
    assert!(!clean().status.success(),"active session must be left alone");
    preview.child.kill().unwrap();preview.child.wait().unwrap();gone(root);gone(child);
    let result=clean();
    assert!(result.status.success(),"inactive owned profile is removable: {}",String::from_utf8_lossy(&result.stderr));
    assert!(!entry.path().exists());
}
