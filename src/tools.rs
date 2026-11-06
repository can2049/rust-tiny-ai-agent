//! 工具层：`Tool` trait、注册表，以及 4 个内置工具。
//!
//! 设计要点（对应需求文档 §5、F3、F6）：
//!
//! - 每个工具只实现 [`Tool`] trait，新增工具只需 [`Registry::register`] 一次，
//!   循环代码（`agent.rs`）不用改。
//! - 工具的 `run` 全部是同步函数，失败一律返回 `Err`，由 [`Registry::execute`]
//!   统一转成 `(错误文本, true)` 回传给模型；参数来自模型（外部输入），
//!   因此这条路径上不允许出现 `unwrap` / `expect`。
//! - 文件工具只能访问 `workspace` 目录之内，越界与符号链接逃逸都会被拒绝。

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::ToolDefinition;

/// `read_file` 单次最多返回的字符数，超过则截断。
const MAX_READ_CHARS: usize = 5000;

/// 天气工具单次 HTTP 请求的超时时间（需求：10 秒）。
const WEATHER_TIMEOUT: Duration = Duration::from_secs(10);

/// Open-Meteo 地理编码接口：把城市名换成经纬度。
const GEOCODING_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";

/// Open-Meteo 预报接口：按经纬度取当前天气。
const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";

// ---------------------------------------------------------------------------
// trait 与注册表
// ---------------------------------------------------------------------------

/// 一个可被模型调用的工具。
///
/// 实现者要保证 `run` 线程安全（会被放进 `Arc<Registry>` 里跨线程使用），
/// 并且对任何外部输入都返回 `Result` 而不是 panic。
pub trait Tool: Send + Sync {
    /// 工具名，模型用它发起调用，全局唯一。
    fn name(&self) -> &str;

    /// 面向模型的说明：什么时候该用、参数格式示例。
    fn description(&self) -> &str;

    /// 参数的 JSON Schema。
    fn input_schema(&self) -> Value;

    /// 执行工具，成功返回结果文本，失败返回错误（由注册表转成错误文本回传）。
    fn run(&self, input: Value) -> Result<String>;
}

/// 工具注册表：按名字保存所有工具，并负责分发调用。
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    /// 创建一个空注册表。
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// 注册一个工具；同名工具会被替换，保证 `definitions` 里名字唯一。
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == name) {
            *slot = tool;
        } else {
            self.tools.push(tool);
        }
    }

    /// 导出所有工具的定义，供 `llm.rs` 组装请求体。
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    /// 注册全部 4 个内置工具，`workspace` 是文件工具的根目录。
    ///
    /// 目录不存在时会尝试创建；创建失败不在这里报错，留给具体的文件工具
    /// 在调用时给出更有针对性的错误。
    pub fn with_builtins(workspace: &Path) -> Self {
        let _ = fs::create_dir_all(workspace);
        // 统一成绝对路径，后面所有越界判断都以它为基准。
        // canonicalize 失败时退化成"当前目录 + 原路径"，保证仍是绝对路径，
        // 否则 starts_with 比较会因一侧为相对路径而误判。
        let workspace = fs::canonicalize(workspace)
            .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(workspace)))
            .unwrap_or_else(|_| workspace.to_path_buf());

        let mut registry = Self::new();
        registry.register(Box::new(Calculator));
        registry.register(Box::new(Weather));
        registry.register(Box::new(ReadFile {
            workspace: workspace.clone(),
        }));
        registry.register(Box::new(WriteFile { workspace }));
        registry
    }

    /// 按名字执行一个工具，返回 `(结果文本, 是否为错误)`。
    ///
    /// 这个方法永不 panic：未知工具、参数错误、工具内部报错都会变成
    /// `is_error = true` 的结果文本，交给模型自行决定重试还是换方案。
    pub fn execute(&self, name: &str, input: Value) -> (String, bool) {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            let names: Vec<&str> = self.tools.iter().map(|tool| tool.name()).collect();
            let available = if names.is_empty() {
                "（无）".to_string()
            } else {
                names.join("，")
            };
            return (format!("未知工具：{name}。可用工具：{available}"), true);
        };

        // 兜底：即便某个工具违反约定 panic 了，也只把它变成一条错误结果。
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tool.run(input))) {
            Ok(Ok(text)) => (text, false),
            Ok(Err(e)) => (format!("工具执行失败：{e}"), true),
            Err(_) => (
                format!("工具执行失败：{name} 内部发生 panic，已被捕获"),
                true,
            ),
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 参数与路径的公共小工具
// ---------------------------------------------------------------------------

/// 从工具输入里取出必填的字符串参数。
fn required_str(input: &Value, field: &str) -> Result<String> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("参数必须是 JSON 对象，收到：{input}"))?;
    match object.get(field) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => bail!("参数 {field} 应该是字符串，收到：{other}"),
        None => bail!("缺少必填参数 {field}"),
    }
}

/// 把模型给出的相对路径解析为 `workspace` 内的绝对路径。
///
/// 做法：先做纯词法规范化（`.` 忽略、`..` 逐级回退且不许越过根），
/// 再沿路径找到最深的已存在祖先并 `canonicalize()`（这一步会解析符号链接），
/// 最后校验结果仍以 `workspace` 开头。尚不存在的文件因此也能安全判定。
fn resolve_in_workspace(workspace: &Path, input_path: &str) -> Result<PathBuf> {
    let raw = input_path.trim();
    if raw.is_empty() {
        bail!("参数 path 不能为空");
    }

    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        bail!("路径越界：path 必须是相对于工作目录的相对路径，收到 {raw}");
    }

    let mut relative = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if !relative.pop() {
                    bail!("路径越界：{raw} 指向工作目录之外");
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("路径越界：{raw} 必须是工作目录内的相对路径");
            }
        }
    }
    if relative.as_os_str().is_empty() {
        bail!("参数 path 指向了工作目录本身，需要给出具体文件名：{raw}");
    }

    // 找出最深的已存在祖先，把它规范化后拼回剩下的（尚不存在的）部分。
    let full = workspace.join(&relative);
    let mut existing = full.clone();
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| anyhow!("路径不合法：{raw}"))?
            .to_os_string();
        tail.push(name);
        existing = existing
            .parent()
            .ok_or_else(|| anyhow!("路径不合法：{raw}"))?
            .to_path_buf();
    }

    let canonical = fs::canonicalize(&existing)
        .with_context(|| format!("无法解析路径：{}", existing.display()))?;
    if !canonical.starts_with(workspace) {
        bail!(
            "路径越界：{raw} 不在工作目录 {} 内",
            workspace.display()
        );
    }

    let mut resolved = canonical;
    for name in tail.iter().rev() {
        resolved.push(name);
    }
    if !resolved.starts_with(workspace) {
        bail!(
            "路径越界：{raw} 不在工作目录 {} 内",
            workspace.display()
        );
    }

    // 目标本身若是符号链接，追一次真实路径，避免写到链接外面去。
    if let Ok(meta) = fs::symlink_metadata(&resolved)
        && meta.file_type().is_symlink()
    {
        let real =
            fs::canonicalize(&resolved).with_context(|| format!("无法解析符号链接：{raw}"))?;
        if !real.starts_with(workspace) {
            bail!("路径越界：{raw} 是符号链接，实际指向 {}", real.display());
        }
        return Ok(real);
    }

    Ok(resolved)
}

/// 把 `f64` 格式化成简洁的可读文本：整数不带小数点，小数去掉多余的尾零。
fn format_number(value: f64) -> String {
    if value == 0.0 {
        // 同时把 -0.0 归成 "0"。
        return "0".to_string();
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let text = format!("{value}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// 1. calculator
// ---------------------------------------------------------------------------

/// calculator 的说明（模型靠它决定要不要调用）。
const CALCULATOR_DESCRIPTION: &str = "\
计算数学表达式并返回精确结果。\
当任务涉及算术运算——加、减、乘、除、括号、小数——时必须调用本工具，\
不要凭心算给出答案，否则容易算错。\
参数示例：{\"expression\": \"(1234 + 5678) * 3.5\"}。\
表达式只允许数字、小数点、+ - * / 和括号（支持一元正负号），除以 0 会报错。";

/// 精确计算数学表达式（手写递归下降解析器，不执行任意代码）。
struct Calculator;

impl Tool for Calculator {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        CALCULATOR_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "要计算的算术表达式，例如 \"(1234 + 5678) * 3.5\"；只允许数字、小数点、+ - * / 和括号"
                }
            },
            "required": ["expression"],
            "additionalProperties": false
        })
    }

    fn run(&self, input: Value) -> Result<String> {
        let expression = required_str(&input, "expression")?;
        let mut parser = Parser::new(&expression)?;
        let value = parser.parse()?;
        if !value.is_finite() {
            bail!("计算结果不是有限数值，可能溢出了：{expression}");
        }
        Ok(format_number(value))
    }
}

/// 算术表达式的递归下降解析器。
///
/// 文法（打印优先级从低到高）：
///
/// ```text
/// expr   := term (('+' | '-') term)*
/// term   := factor (('*' | '/') factor)*
/// factor := ['+' | '-'] (number | '(' expr ')')
/// ```
struct Parser {
    /// 已过滤掉空白的字符序列（因此下标就是"第几个有效字符"）。
    chars: Vec<char>,
    /// 当前读取位置。
    pos: usize,
}

impl Parser {
    /// 校验字符集并构造解析器：只接受数字、`.`、`+ - * / ( )` 和空白。
    fn new(source: &str) -> Result<Self> {
        let mut chars = Vec::with_capacity(source.len());
        for ch in source.chars() {
            if ch.is_whitespace() {
                continue;
            }
            if !matches!(ch, '0'..='9' | '.' | '+' | '-' | '*' | '/' | '(' | ')') {
                bail!("表达式包含不允许的字符 '{ch}'；只允许数字、小数点、+ - * /、括号和空白字符");
            }
            chars.push(ch);
        }
        Ok(Self { chars, pos: 0 })
    }

    /// 读取下一个有效字符但不前进。
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// 解析整个表达式，并要求输入被完全消费。
    fn parse(&mut self) -> Result<f64> {
        if self.chars.is_empty() {
            bail!("表达式为空，请给出例如 \"(1 + 2) * 3\" 的算式");
        }
        let value = self.expr()?;
        if let Some(ch) = self.peek() {
            bail!("表达式在第 {} 个有效字符处有多余内容：'{ch}'", self.pos + 1);
        }
        Ok(value)
    }

    /// `expr := term (('+' | '-') term)*`
    fn expr(&mut self) -> Result<f64> {
        let mut acc = self.term()?;
        while let Some(op) = self.peek() {
            match op {
                '+' => {
                    self.pos += 1;
                    acc += self.term()?;
                }
                '-' => {
                    self.pos += 1;
                    acc -= self.term()?;
                }
                _ => break,
            }
        }
        Ok(acc)
    }

    /// `term := factor (('*' | '/') factor)*`
    fn term(&mut self) -> Result<f64> {
        let mut acc = self.factor()?;
        while let Some(op) = self.peek() {
            match op {
                '*' => {
                    self.pos += 1;
                    acc *= self.factor()?;
                }
                '/' => {
                    self.pos += 1;
                    let divisor = self.factor()?;
                    if divisor == 0.0 {
                        bail!("除数不能为 0");
                    }
                    acc /= divisor;
                }
                _ => break,
            }
        }
        Ok(acc)
    }

    /// `factor := ['+' | '-'] (number | '(' expr ')')`
    fn factor(&mut self) -> Result<f64> {
        match self.peek() {
            Some('+') => {
                self.pos += 1;
                self.factor()
            }
            Some('-') => {
                self.pos += 1;
                let value = self.factor()?;
                Ok(-value)
            }
            Some('(') => {
                self.pos += 1;
                let value = self.expr()?;
                match self.peek() {
                    Some(')') => {
                        self.pos += 1;
                        Ok(value)
                    }
                    _ => bail!("缺少右括号 ')'"),
                }
            }
            Some(ch) if ch.is_ascii_digit() || ch == '.' => self.number(),
            Some(ch) => bail!("表达式语法错误：'{ch}' 不能出现在第 {} 个有效字符处", self.pos + 1),
            None => bail!("表达式意外结束，这里应该是数字或左括号"),
        }
    }

    /// 读取一个十进制数（允许 `12`、`1.5`、`.5`、`1.` 这些写法）。
    fn number(&mut self) -> Result<f64> {
        let start = self.pos;
        let mut seen_digit = false;
        let mut seen_dot = false;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                seen_digit = true;
                self.pos += 1;
            } else if ch == '.' {
                if seen_dot {
                    bail!("数字里出现了多个小数点（第 {} 个有效字符处）", self.pos + 1);
                }
                seen_dot = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        if !seen_digit {
            bail!("第 {} 个有效字符处应该是一个数字", start + 1);
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map_err(|e| anyhow!("无法解析数字 {text}：{e}"))
    }
}

// ---------------------------------------------------------------------------
// 2. get_weather
// ---------------------------------------------------------------------------

/// get_weather 的说明。
const WEATHER_DESCRIPTION: &str = "\
查询某个城市当前的实时天气（气温、体感温度、湿度、天气状况）。\
当任务需要此刻的实时天气（气温、体感、湿度）时必须调用本工具，不要凭记忆编造；\
只需要历史常识或与天气无关的问题时不要调用。\
参数示例：{\"city\": \"香港\"}，城市名可以是中文或英文。";

/// 通过 Open-Meteo 免费接口查询城市当前天气。
struct Weather;

/// 地理编码接口的响应。
#[derive(Deserialize)]
struct GeoResponse {
    /// 匹配到的地点列表，查不到时是 `null` 或缺省。
    results: Option<Vec<GeoPlace>>,
}

/// 地理编码结果中的单个地点。
#[derive(Deserialize)]
struct GeoPlace {
    /// 地点名（请求时带 `language=zh`，所以通常是中文）。
    name: String,
    /// 纬度。
    latitude: f64,
    /// 经度。
    longitude: f64,
}

/// 预报接口的响应，只关心 `current` 部分。
#[derive(Deserialize)]
struct ForecastResponse {
    /// 当前天气，接口异常时可能缺省。
    current: Option<CurrentWeather>,
}

/// `current` 字段的内容。
#[derive(Deserialize)]
struct CurrentWeather {
    /// 2 米气温（摄氏度）。
    temperature_2m: f64,
    /// 体感温度（摄氏度）。
    apparent_temperature: f64,
    /// 相对湿度（百分比）。
    relative_humidity_2m: f64,
    /// WMO 天气编码。
    weather_code: i64,
}

impl Weather {
    /// 第一步：城市名 → 经纬度。
    fn geocode(client: &reqwest::blocking::Client, city: &str) -> Result<GeoPlace> {
        let response = client
            .get(GEOCODING_URL)
            .query(&[
                ("name", city),
                ("count", "1"),
                ("language", "zh"),
                ("format", "json"),
            ])
            .send()
            .with_context(|| format!("请求地理编码接口失败（城市：{city}）"))?;

        let status = response.status();
        let body = response
            .text()
            .context("读取地理编码接口响应失败")?;
        if !status.is_success() {
            bail!("地理编码接口返回 HTTP {status}：{}", body.trim());
        }

        let parsed: GeoResponse = serde_json::from_str(&body)
            .with_context(|| format!("解析地理编码响应失败：{}", body.trim()))?;
        parsed
            .results
            .and_then(|mut list| {
                if list.is_empty() {
                    None
                } else {
                    Some(list.remove(0))
                }
            })
            .ok_or_else(|| anyhow!("未找到城市：{city}"))
    }

    /// 第二步：经纬度 → 当前天气。
    fn forecast(
        client: &reqwest::blocking::Client,
        place: &GeoPlace,
    ) -> Result<CurrentWeather> {
        let latitude = format!("{:.6}", place.latitude);
        let longitude = format!("{:.6}", place.longitude);

        let response = client
            .get(FORECAST_URL)
            .query(&[
                ("latitude", latitude.as_str()),
                ("longitude", longitude.as_str()),
                (
                    "current",
                    "temperature_2m,apparent_temperature,relative_humidity_2m,weather_code",
                ),
                ("timezone", "auto"),
            ])
            .send()
            .with_context(|| format!("请求天气接口失败（城市：{}）", place.name))?;

        let status = response.status();
        let body = response.text().context("读取天气接口响应失败")?;
        if !status.is_success() {
            bail!("天气接口返回 HTTP {status}：{}", body.trim());
        }

        let parsed: ForecastResponse =
            serde_json::from_str(&body).with_context(|| format!("解析天气响应失败：{}", body.trim()))?;
        parsed
            .current
            .ok_or_else(|| anyhow!("天气接口没有返回当前天气（城市：{}）", place.name))
    }
}

impl Tool for Weather {
    fn name(&self) -> &str {
        "get_weather"
    }

    fn description(&self) -> &str {
        WEATHER_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "城市名称，中文或英文均可，例如 \"香港\" 或 \"Hong Kong\""
                }
            },
            "required": ["city"],
            "additionalProperties": false
        })
    }

    fn run(&self, input: Value) -> Result<String> {
        let city = required_str(&input, "city")?;
        let city = city.trim();
        if city.is_empty() {
            bail!("参数 city 不能为空");
        }

        // 客户端必须在本次调用内创建、用完即 drop：
        // reqwest 的阻塞客户端会自带一个 tokio 运行时，跨线程长期持有会 panic。
        let client = reqwest::blocking::Client::builder()
            .timeout(WEATHER_TIMEOUT)
            .build()
            .context("创建 HTTP 客户端失败")?;

        let place = Weather::geocode(&client, city)?;
        let current = Weather::forecast(&client, &place)?;

        Ok(format!(
            "{}：气温 {}°C（体感 {}°C），湿度 {}%，天气：{}",
            place.name,
            format_number(current.temperature_2m),
            format_number(current.apparent_temperature),
            format_number(current.relative_humidity_2m),
            wmo_description(current.weather_code)
        ))
    }
}

/// 把 WMO 天气编码翻成中文描述，未知编码给"未知"。
fn wmo_description(code: i64) -> String {
    let text = match code {
        0 => "晴",
        1 => "晴间多云",
        2 => "多云",
        3 => "阴",
        45 | 48 => "雾",
        51 | 53 | 55 => "毛毛雨",
        56 | 57 => "冻毛毛雨",
        61 => "小雨",
        63 => "中雨",
        65 => "大雨",
        66 | 67 => "冻雨",
        71 => "小雪",
        73 => "中雪",
        75 => "大雪",
        77 => "雪粒",
        80 => "阵雨",
        81 => "强阵雨",
        82 => "暴雨",
        85 => "阵雪",
        86 => "强阵雪",
        95 => "雷阵雨",
        96 | 99 => "雷阵雨伴冰雹",
        _ => "未知",
    };
    text.to_string()
}

// ---------------------------------------------------------------------------
// 3. read_file
// ---------------------------------------------------------------------------

/// read_file 的说明。
const READ_FILE_DESCRIPTION: &str = "\
读取工作目录内的文本文件。\
当任务需要查看已有文件的内容、核对之前写下的信息时调用；\
只知道文件名而不知道内容时必须调用，不要猜测文件内容。\
参数示例：{\"path\": \"notes.txt\"}，path 是相对于工作目录的相对路径，不能使用 ../ 或绝对路径。\
超过 5000 字符的内容会被截断并注明原长度。";

/// 读取工作目录内的文本文件（超长自动截断）。
struct ReadFile {
    /// 文件工具唯一可访问的根目录（已规范化）。
    workspace: PathBuf,
}

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        READ_FILE_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "相对于工作目录的文件路径，例如 \"notes.txt\" 或 \"data/log.txt\""
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn run(&self, input: Value) -> Result<String> {
        let raw_path = required_str(&input, "path")?;
        let path = resolve_in_workspace(&self.workspace, &raw_path)?;

        if !path.exists() {
            bail!("文件不存在：{raw_path}（工作目录 {} 内没有这个文件）", self.workspace.display());
        }
        if path.is_dir() {
            bail!("{raw_path} 是一个目录，不是文本文件");
        }

        let bytes = fs::read(&path).with_context(|| format!("读取文件失败：{raw_path}"))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| anyhow!("文件不是合法的 UTF-8 文本，无法读取：{raw_path}"))?;

        let total = content.chars().count();
        if total <= MAX_READ_CHARS {
            return Ok(content);
        }

        let head: String = content.chars().take(MAX_READ_CHARS).collect();
        Ok(format!(
            "{head}\n[内容已截断，原长度 {total} 字符，仅显示前 {MAX_READ_CHARS} 字符]"
        ))
    }
}

// ---------------------------------------------------------------------------
// 4. write_file
// ---------------------------------------------------------------------------

/// write_file 的说明。
const WRITE_FILE_DESCRIPTION: &str = "\
把文本内容写入工作目录内的文件（自动创建父目录，已有同名文件会被覆盖）。\
当任务要求保存结果、记录信息到文件时必须调用；只是把内容说给用户听时不要调用。\
参数示例：{\"path\": \"weather.txt\", \"content\": \"香港：气温 25.9°C\"}，\
path 是相对于工作目录的相对路径，不能使用 ../ 或绝对路径。";

/// 把文本写入工作目录内的文件。
struct WriteFile {
    /// 文件工具唯一可访问的根目录（已规范化）。
    workspace: PathBuf,
}

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        WRITE_FILE_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "相对于工作目录的文件路径，例如 \"weather.txt\""
                },
                "content": {
                    "type": "string",
                    "description": "要写入的完整文本内容（会覆盖文件原有内容）"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn run(&self, input: Value) -> Result<String> {
        let raw_path = required_str(&input, "path")?;
        let content = required_str(&input, "content")?;
        let path = resolve_in_workspace(&self.workspace, &raw_path)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建父目录失败：{}", parent.display()))?;
        }
        fs::write(&path, &content).with_context(|| format!("写入文件失败：{raw_path}"))?;

        Ok(format!(
            "已写入 {} 字符到 {}",
            content.chars().count(),
            raw_path
        ))
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个只给本模块用的临时工作目录。
    fn temp_workspace(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "tiny_ai_agent_tools_{tag}_{}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("创建临时目录失败");
        fs::canonicalize(&dir).expect("规范化临时目录失败")
    }

    /// 只注册 calculator 的注册表。
    fn calculator_registry() -> Registry {
        let mut registry = Registry::new();
        registry.register(Box::new(Calculator));
        registry
    }

    #[test]
    fn calculator_matches_acceptance_example() {
        let registry = calculator_registry();
        let (text, is_error) = registry.execute(
            "calculator",
            json!({"expression": "(1234 + 5678) * 3.5"}),
        );
        assert!(!is_error, "不应报错：{text}");
        assert_eq!(text, "24192");
    }

    #[test]
    fn calculator_handles_precedence_and_unary() {
        let registry = calculator_registry();
        let cases = [
            ("1 + 2 * 3", "7"),
            ("(1 + 2) * 3", "9"),
            ("-3 + 1", "-2"),
            ("10 / 4", "2.5"),
            ("--2", "2"),
            ("2 * -3", "-6"),
            ("1.50 + 1.50", "3"),
            (".5 + .5", "1"),
        ];
        for (expression, expected) in cases {
            let (text, is_error) = registry.execute("calculator", json!({"expression": expression}));
            assert!(!is_error, "{expression} 不应报错：{text}");
            assert_eq!(text, expected, "表达式 {expression}");
        }
    }

    #[test]
    fn calculator_rejects_bad_input() {
        let registry = calculator_registry();
        let bad = [
            json!({"expression": "1 / 0"}),
            json!({"expression": "1 +"}),
            json!({"expression": "(1 + 2"}),
            json!({"expression": "2 ^ 3"}),
            json!({"expression": "1; drop table"}),
            json!({"expression": ""}),
            json!({"expression": 42}),
            json!({"expression": "1.2.3"}),
            json!({}),
            json!(null),
        ];
        for input in bad {
            let (text, is_error) = registry.execute("calculator", input.clone());
            assert!(is_error, "输入 {input} 应该失败，实际返回：{text}");
            assert!(text.contains("工具执行失败"), "错误文本应统一前缀：{text}");
        }
    }

    #[test]
    fn unknown_tool_is_reported() {
        let registry = calculator_registry();
        let (text, is_error) = registry.execute("rm_rf", json!({}));
        assert!(is_error);
        assert!(text.contains("未知工具：rm_rf"), "{text}");
        assert!(text.contains("calculator"), "应列出可用工具：{text}");
    }

    #[test]
    fn definitions_expose_name_description_and_schema() {
        let workspace = temp_workspace("defs");
        let registry = Registry::with_builtins(&workspace);
        let definitions = registry.definitions();
        let names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["calculator", "get_weather", "read_file", "write_file"]
        );
        for definition in &definitions {
            assert!(!definition.description.is_empty());
            assert_eq!(definition.input_schema["type"], "object");
        }
        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn write_then_read_roundtrip() {
        let workspace = temp_workspace("roundtrip");
        let registry = Registry::with_builtins(&workspace);

        let (text, is_error) = registry.execute(
            "write_file",
            json!({"path": "sub/note.txt", "content": "你好 agent"}),
        );
        assert!(!is_error, "{text}");
        assert_eq!(text, "已写入 8 字符到 sub/note.txt");

        let (read_back, is_error) = registry.execute("read_file", json!({"path": "sub/note.txt"}));
        assert!(!is_error, "{read_back}");
        assert_eq!(read_back, "你好 agent");

        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn read_file_truncates_long_content() {
        let workspace = temp_workspace("truncate");
        let registry = Registry::with_builtins(&workspace);
        let long = "汉".repeat(MAX_READ_CHARS + 100);

        let (text, is_error) =
            registry.execute("write_file", json!({"path": "long.txt", "content": long}));
        assert!(!is_error, "{text}");

        let (read_back, is_error) = registry.execute("read_file", json!({"path": "long.txt"}));
        assert!(!is_error, "{read_back}");
        assert!(read_back.contains("内容已截断"), "{read_back}");
        assert!(
            read_back.contains(&format!("原长度 {}", MAX_READ_CHARS + 100)),
            "{read_back}"
        );
        assert_eq!(read_back.chars().filter(|c| *c == '汉').count(), MAX_READ_CHARS);

        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let workspace = temp_workspace("missing");
        let registry = Registry::with_builtins(&workspace);
        let (text, is_error) = registry.execute("read_file", json!({"path": "nope.txt"}));
        assert!(is_error);
        assert!(text.contains("文件不存在"), "{text}");
        fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn path_escape_is_rejected() {
        let workspace = temp_workspace("escape");
        let registry = Registry::with_builtins(&workspace);
        let attempts = [
            "../secret.txt",
            "../../etc/passwd",
            "sub/../../outside.txt",
            "/etc/passwd",
            "",
        ];
        for path in attempts {
            let (text, is_error) = registry.execute("read_file", json!({"path": path}));
            assert!(is_error, "路径 {path} 应被拒绝");
            assert!(
                text.contains("越界") || text.contains("不能为空"),
                "错误信息应说明原因：{text}"
            );

            let (text, is_error) =
                registry.execute("write_file", json!({"path": path, "content": "x"}));
            assert!(is_error, "写入路径 {path} 应被拒绝");
            assert!(
                text.contains("越界") || text.contains("不能为空"),
                "错误信息应说明原因：{text}"
            );
        }
        assert!(!workspace.join("../secret.txt").exists());
        fs::remove_dir_all(&workspace).ok();
    }

    /// 符号链接逃逸必须被拒绝（需求 §6：拒绝越界和符号链接逃逸）。
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = temp_workspace("symlink");
        let outside = temp_workspace("symlink_outside");
        fs::write(outside.join("secret.txt"), "机密").expect("写入外部文件失败");

        // workspace/link -> 外部目录
        symlink(&outside, workspace.join("link")).expect("创建目录符号链接失败");
        // workspace/leak.txt -> 外部文件
        symlink(outside.join("secret.txt"), workspace.join("leak.txt"))
            .expect("创建文件符号链接失败");

        let registry = Registry::with_builtins(&workspace);

        let (text, is_error) = registry.execute("read_file", json!({"path": "link/secret.txt"}));
        assert!(is_error, "通过目录符号链接读外部文件应被拒绝：{text}");
        assert!(text.contains("越界"), "{text}");

        let (text, is_error) = registry.execute("read_file", json!({"path": "leak.txt"}));
        assert!(is_error, "通过文件符号链接读外部文件应被拒绝：{text}");
        assert!(text.contains("越界"), "{text}");

        let (text, is_error) =
            registry.execute("write_file", json!({"path": "link/evil.txt", "content": "x"}));
        assert!(is_error, "通过符号链接写外部文件应被拒绝：{text}");
        assert!(text.contains("越界"), "{text}");
        assert!(!outside.join("evil.txt").exists(), "外部文件不应被创建");

        fs::remove_dir_all(&workspace).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn number_formatting_is_compact() {
        assert_eq!(format_number(24192.0), "24192");
        assert_eq!(format_number(3.5), "3.5");
        assert_eq!(format_number(25.90), "25.9");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(-3.0), "-3");
    }

    #[test]
    fn wmo_codes_map_to_chinese() {
        assert_eq!(wmo_description(0), "晴");
        assert_eq!(wmo_description(1), "晴间多云");
        assert_eq!(wmo_description(63), "中雨");
        assert_eq!(wmo_description(99), "雷阵雨伴冰雹");
        assert_eq!(wmo_description(12345), "未知");
    }

    /// 真正访问 Open-Meteo 的联网测试，默认忽略：`cargo test -- --ignored weather`。
    ///
    /// 这里特意走 `spawn_blocking`，复现 `agent.rs` 的真实调用路径：
    /// 阻塞式 HTTP 客户端在阻塞线程里创建和销毁，不能在异步线程上构建。
    #[test]
    #[ignore = "需要联网访问 Open-Meteo"]
    fn weather_end_to_end() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("创建测试运行时失败");

        let registry = std::sync::Arc::new(Registry::with_builtins(&temp_workspace("weather")));

        let (text, is_error) = runtime.block_on(async {
            let registry = std::sync::Arc::clone(&registry);
            tokio::task::spawn_blocking(move || {
                registry.execute("get_weather", json!({"city": "Hong Kong"}))
            })
            .await
            .expect("阻塞任务不应 panic")
        });
        assert!(!is_error, "{text}");
        assert!(text.contains('°'), "{text}");
        println!("{text}");

        let (text, is_error) = runtime.block_on(async {
            let registry = std::sync::Arc::clone(&registry);
            tokio::task::spawn_blocking(move || {
                registry.execute("get_weather", json!({"city": "zzzz_not_a_city"}))
            })
            .await
            .expect("阻塞任务不应 panic")
        });
        assert!(is_error);
        assert!(text.contains("未找到城市"), "{text}");
    }
}