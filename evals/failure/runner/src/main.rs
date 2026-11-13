//! 失败面题集 runner。
//!
//! 用法（务必用独立 target 目录，避免占用主 target 锁）：
//! ```powershell
//! $env:CARGO_TARGET_DIR = "D:\MyWorkspace\SHANIU\target\evals-failure"
//! cargo run --manifest-path evals/failure/runner/Cargo.toml --offline
//! ```
//!
//! 退出码：0 = 套件跑完且每题都拿到了判据结论（红也是结论）；2 = 基础设施错误
//! （mock/判据脚本/观测缺失）。题目红绿由 `judge/run.ps1` 裁决，runner 不替判据说话。

mod mock;
mod scenario;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio 运行时创建失败");
    let code = runtime.block_on(run());
    std::process::exit(code);
}

async fn run() -> i32 {
    let mut tasks_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("failure-tasks.json");
    let mut only: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tasks" => {
                if let Some(value) = args.next() {
                    tasks_path = PathBuf::from(value);
                }
            }
            "--only" => {
                if let Some(value) = args.next() {
                    only.push(value);
                }
            }
            "--help" | "-h" => {
                println!("用法: failure-suite [--tasks <failure-tasks.json>] [--only <task-id>]...");
                return 0;
            }
            other => {
                eprintln!("未知参数: {other}（--help 查看用法）");
                return 2;
            }
        }
    }

    let tasks_path = normalize(&tasks_path);
    let text = match std::fs::read_to_string(&tasks_path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("读取题单失败 {}: {error}", tasks_path.display());
            return 2;
        }
    };
    let file: scenario::TaskFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("题单解析失败 {}: {error}", tasks_path.display());
            return 2;
        }
    };

    let repo_root = tasks_path
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let repo_commit = git_commit(&repo_root);
    let run_id = format!("{}-{}", scenario::now_ms(), std::process::id());
    let results_dir = scenario::results_dir_for(&tasks_path);
    if let Err(error) = std::fs::create_dir_all(&results_dir) {
        eprintln!("创建 results 目录失败 {}: {error}", results_dir.display());
        return 2;
    }
    let work_root = std::env::temp_dir().join(format!("shaniu-failure-{run_id}"));
    if let Err(error) = std::fs::create_dir_all(&work_root) {
        eprintln!("创建工作目录失败 {}: {error}", work_root.display());
        return 2;
    }

    println!(
        "失败面题集 run_id={run_id} commit={repo_commit} work={}",
        work_root.display()
    );

    let mut observations: Vec<Value> = Vec::new();
    let mut infra_error = false;
    for spec in &file.tasks {
        if !only.is_empty() && !only.iter().any(|id| id == &spec.id) {
            continue;
        }
        print!("运行 {} ... ", spec.id);
        match scenario::run(spec, &run_id, &work_root, &repo_commit).await {
            Ok(observation) => {
                println!("完成");
                observations.push(observation);
            }
            Err(error) => {
                println!("观测失败");
                eprintln!("  {error}");
                infra_error = true;
            }
        }
    }

    let result_path = results_dir.join(format!("{run_id}-failure.json"));
    let mut document = json!({
        "suite": "failure",
        "version": 1,
        "run_id": run_id,
        "repo_commit": repo_commit,
        "generated_at_ms": scenario::now_ms(),
        "tasks": observations,
    });
    if let Err(error) = write_json(&result_path, &document) {
        eprintln!("写观测文件失败 {}: {error}", result_path.display());
        return 2;
    }

    let judge_path = repo_root.join("evals").join("failure").join("judge").join("run.ps1");
    if !judge_path.is_file() {
        eprintln!("判据脚本不存在: {}", judge_path.display());
        return 2;
    }

    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    let tasks = document["tasks"].as_array_mut().expect("tasks 必须是数组");
    for observation in tasks.iter_mut() {
        let id = observation["id"].as_str().unwrap_or("?").to_string();
        let expect = observation["expect"].as_str().unwrap_or("pass").to_string();
        let verdict = match call_judge(&judge_path, &id, &result_path) {
            Ok(verdict) => verdict,
            Err(error) => {
                eprintln!("判据调用失败 {id}: {error}");
                observation["verdict"] = json!({"status": "INFRA", "message": error});
                rows.push((id, expect, "INFRA".to_string(), format!("判据未执行: {error}")));
                infra_error = true;
                continue;
            }
        };
        let status = verdict["status"].as_str().unwrap_or("FAIL").to_string();
        let note = match (expect.as_str(), status.as_str()) {
            ("pass", "PASS") => "通过".to_string(),
            ("pass", "FAIL") => "意外失败（需排查）".to_string(),
            ("gap", "FAIL") => "缺口确认（符合登记）".to_string(),
            ("gap", "PASS") => "缺口已修复（请更新登记）".to_string(),
            _ => "判据未给出 PASS/FAIL".to_string(),
        };
        observation["verdict"] = verdict;
        rows.push((id, expect, status, note));
    }

    if let Err(error) = write_json(&result_path, &document) {
        eprintln!("回写观测文件失败 {}: {error}", result_path.display());
        return 2;
    }

    println!();
    println!("{:<44} {:<6} {:<6} {}", "task", "expect", "judge", "note");
    for (id, expect, status, note) in &rows {
        println!("{id:<44} {expect:<6} {status:<6} {note}");
    }
    println!();
    println!("观测文件: {}", result_path.display());
    if infra_error {
        println!("结论: 存在基础设施错误，本批不可判定");
        2
    } else {
        println!("结论: 本批已全部裁决（红绿见上表；红不代表套件故障）");
        0
    }
}

fn call_judge(judge: &Path, task_id: &str, observation: &Path) -> Result<Value, String> {
    let attempts: [(&str, Vec<String>); 2] = [
        (
            "pwsh",
            vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                judge.display().to_string(),
                "-TaskId".into(),
                task_id.into(),
                "-Observation".into(),
                observation.display().to_string(),
            ],
        ),
        (
            "powershell",
            vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                judge.display().to_string(),
                "-TaskId".into(),
                task_id.into(),
                "-Observation".into(),
                observation.display().to_string(),
            ],
        ),
    ];
    let mut last_error = String::new();
    for (program, args) in attempts {
        match Command::new(program).args(&args).output() {
            Ok(output) => return Ok(verdict_from(output)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_error = format!("{program} 未安装");
            }
            Err(error) => return Err(format!("{program} 启动失败: {error}")),
        }
    }
    Err(last_error)
}

fn verdict_from(output: Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}{stderr}");
    let status = if combined.contains("REPO-CHECK: PASS") { "PASS" } else { "FAIL" };
    json!({
        "status": status,
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
    })
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| error.to_string())
}

fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn git_commit(repo_root: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
