//! 运行日志：把每个关键步骤写成一行 JSON（JSONL），便于复盘与排障。
//!
//! 为什么是 JSONL：一行一个事件，既能 `cat` 着看，也能直接 `jq` 过滤/统计；
//! 追加写入、逐行 flush，所以进程被 Ctrl+C 或崩溃时，已发生的事件不会丢。
//!
//! 日志里**绝不包含 API Key**（Key 只在 HTTP 头里，不进入任何被记录的结构）。
//! 日志文件默认写到 `logs/agent-<时间戳>-<pid>.log`，用 `AGENT_LOG_FILE=0` 关闭。

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use chrono::SecondsFormat;
use serde_json::{Map, Value, json};

use crate::config::Config;

/// 不属于任何一次任务的事件（例如会话启动）使用的 run 编号。
pub const NO_RUN: u64 = 0;

/// 一次会话的事件记录器。`Arc<Logger>` 在 `main` / `agent` / `llm` 之间共享。
pub struct Logger {
    /// 日志文件；`None` 表示不写文件（`AGENT_LOG_FILE=0`）。
    sink: Option<Mutex<BufWriter<File>>>,
    /// 日志文件路径，供启动时打印给用户。
    path: Option<PathBuf>,
    /// 事件全局自增序号，用来还原严格顺序。
    seq: AtomicU64,
    /// 当前任务编号，每次 `begin_run` 自增。
    run: AtomicU64,
    /// 会话标识，同一个进程内所有事件相同。
    session: String,
    /// 单条内容字段最多保留的字符数（超出截断并注明原长度）。
    content_limit: usize,
    /// IO 错误只提示一次，避免刷屏。
    warned: AtomicBool,
}

impl Logger {
    /// 按配置创建日志器；关闭文件日志时返回一个什么都不做的实例。
    pub fn new(cfg: &Config) -> Result<Self> {
        let session = chrono::Local::now().to_rfc3339_opts(SecondsFormat::Millis, false);
        if !cfg.log_file_enabled {
            return Ok(Self::disabled(cfg.log_content_limit));
        }

        fs::create_dir_all(&cfg.log_dir)
            .with_context(|| format!("创建日志目录失败：{}", cfg.log_dir.display()))?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = cfg
            .log_dir
            .join(format!("agent-{stamp}-{}.log", std::process::id()));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开日志文件失败：{}", path.display()))?;

        Ok(Self {
            sink: Some(Mutex::new(BufWriter::new(file))),
            path: Some(path),
            seq: AtomicU64::new(0),
            run: AtomicU64::new(NO_RUN),
            session,
            content_limit: cfg.log_content_limit,
            warned: AtomicBool::new(false),
        })
    }

    /// 构造一个不写文件的日志器（测试用）。
    pub fn disabled(content_limit: usize) -> Self {
        Self {
            sink: None,
            path: None,
            seq: AtomicU64::new(0),
            run: AtomicU64::new(NO_RUN),
            session: chrono::Local::now().to_rfc3339_opts(SecondsFormat::Millis, false),
            content_limit,
            warned: AtomicBool::new(false),
        }
    }

    /// 日志文件路径（未启用文件日志时为 `None`）。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 开始一次新任务，返回其编号；之后的事件都会带上它。
    pub fn begin_run(&self, task: &str) -> u64 {
        let run = self.run.fetch_add(1, Ordering::SeqCst) + 1;
        self.emit(
            None,
            "task_start",
            json!({
                "run": run,
                "task": task,
                "task_chars": task.chars().count(),
            }),
        );
        run
    }

    /// 记录一个事件。`data` 必须是 JSON 对象，其字段会并入事件顶层。
    pub fn emit(&self, step: Option<usize>, event: &str, data: Value) {
        if self.sink.is_none() {
            return;
        }

        let mut obj = Map::new();
        obj.insert("ts".into(), json!(chrono::Local::now().to_rfc3339_opts(SecondsFormat::Millis, false)));
        obj.insert("seq".into(), json!(self.seq.fetch_add(1, Ordering::SeqCst) + 1));
        obj.insert("session".into(), json!(self.session));
        obj.insert("run".into(), json!(self.run.load(Ordering::SeqCst)));
        if let Some(step) = step {
            obj.insert("step".into(), json!(step));
        }
        obj.insert("event".into(), json!(event));
        if let Value::Object(extra) = data {
            for (key, value) in extra {
                obj.insert(key, value);
            }
        }

        // 事件里只可能包含我们自己构造的 JSON，序列化不会失败；
        // 万一失败用 `to_string` 兜底，保证不 panic。
        let line = Value::Object(obj).to_string();
        self.write_line(&line);
    }

    /// 截断一段内容，并附上原始字符数（用于工具结果等可能很长的文本）。
    pub fn clip(&self, content: &str) -> Value {
        let total = content.chars().count();
        if total <= self.content_limit {
            json!({ "content": content, "content_chars": total, "truncated": false })
        } else {
            let head: String = content.chars().take(self.content_limit).collect();
            json!({ "content": head, "content_chars": total, "truncated": true })
        }
    }

    /// 追加一行并立即 flush，保证崩溃前的日志已落盘。
    fn write_line(&self, line: &str) {
        let Some(sink) = &self.sink else { return };
        // 本进程内不存在跨线程的写日志嵌套，锁中毒说明之前某次写入 panic 了，
        // 这里继续用内部数据写，不要升级成二次 panic。
        let mut guard = match sink.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let result = guard
            .write_all(line.as_bytes())
            .and_then(|()| guard.write_all(b"\n"))
            .and_then(|()| guard.flush());
        if let Err(err) = result {
            drop(guard);
            if !self.warned.swap(true, Ordering::SeqCst) {
                eprintln!("[日志] 写入日志文件失败（后续不再提示）：{err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentlog_{tag}_{}_{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn logger_in(dir: &Path) -> Logger {
        let cfg = Config {
            model: "m".into(),
            base_url: "https://example.com/v1".into(),
            api_key: "sk-secret-should-never-appear".into(),
            max_steps: 3,
            max_tokens: 100,
            workspace: dir.to_path_buf(),
            system_prompt: "sys".into(),
            log_enabled: true,
            log_dir: dir.to_path_buf(),
            log_file_enabled: true,
            log_content_limit: 10,
        };
        Logger::new(&cfg).expect("创建日志器")
    }

    #[test]
    fn writes_one_json_object_per_line() {
        let dir = temp_dir("jsonl");
        let logger = logger_in(&dir);
        let run = logger.begin_run("算一下 1+1");
        logger.emit(Some(1), "llm_request", json!({"message_count": 2}));
        logger.emit(Some(1), "tool_call", json!({"tool": "calculator"}));
        logger.emit(None, "task_end", json!({"outcome": "ok", "run": run}));

        let text = fs::read_to_string(logger.path().expect("有日志文件")).expect("读日志");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "task_start + 3 个事件");

        for line in &lines {
            let value: Value = serde_json::from_str(line).expect("每行都是合法 JSON");
            assert!(value.get("ts").is_some());
            assert!(value.get("seq").is_some());
            assert!(value.get("session").is_some());
            assert!(value.get("event").is_some());
        }

        let first: Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(first["event"], "task_start");
        assert_eq!(first["run"], run);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let dir = temp_dir("seq");
        let logger = logger_in(&dir);
        for i in 0..5 {
            logger.emit(Some(i), "step", json!({}));
        }
        let text = fs::read_to_string(logger.path().expect("有日志文件")).expect("读日志");
        let seqs: Vec<u64> = text
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).expect("json")["seq"]
                    .as_u64()
                    .expect("seq 是数字")
            })
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clip_truncates_and_reports_original_length() {
        let dir = temp_dir("clip");
        let logger = logger_in(&dir); // content_limit = 10
        let short = logger.clip("短");
        assert_eq!(short["truncated"], false);
        assert_eq!(short["content_chars"], 1);

        let long = logger.clip("一二三四五六七八九十十一");
        assert_eq!(long["truncated"], true);
        assert_eq!(long["content_chars"], 12);
        assert_eq!(long["content"].as_str().expect("字符串").chars().count(), 10);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_logger_writes_nothing() {
        let logger = Logger::disabled(100);
        logger.begin_run("无所谓");
        logger.emit(None, "event", json!({}));
        assert!(logger.path().is_none());
    }

    #[test]
    fn api_key_never_reaches_the_log() {
        let dir = temp_dir("secret");
        let logger = logger_in(&dir);
        logger.begin_run("任务");
        logger.emit(None, "llm_request", json!({"url": "https://example.com/v1"}));
        let text = fs::read_to_string(logger.path().expect("有日志文件")).expect("读日志");
        assert!(!text.contains("sk-secret-should-never-appear"));
        let _ = fs::remove_dir_all(&dir);
    }
}
