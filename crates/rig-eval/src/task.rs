//! 评估任务定义。任务文件放在 `eval/tasks/<scenario>/*.toml`，页面放在 `eval/fixtures/<scenario>/`。

use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    Web,
    Android,
}

/// 成功判据。尽量用确定性检查，不用模型打分。
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Success {
    /// 页面中某元素的文本包含给定内容。`frame` 为 iframe 的选择器（可选）。
    DomText {
        selector: String,
        contains: String,
        #[serde(default)]
        frame: Option<String>,
    },
    /// 页面中某元素不存在。
    ElementAbsent { selector: String },
    /// 全部子判据都成立。
    All { checks: Vec<Success> },
    /// 需要人工判断（只用于暂时无法自动判定的任务）。
    Manual { note: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub scenario: Scenario,
    /// 相对 `eval/fixtures/<scenario>/` 的页面文件。
    #[serde(default)]
    pub fixture: Option<String>,
    /// 给 AI 的目标描述。
    pub goal: String,
    /// 任务涉及的秘密值。**不得出现在任何 Observation、日志、模型输入中**（见 `leak`）。
    #[serde(default)]
    pub secrets: Vec<String>,
    /// 可信执行器专用的凭据（按引用名注入，如登录的 user/pass）。
    /// 只存在于可信侧：其值与 `secrets` 一样，**永不进入模型输入**；`goal` 中只能以引用名提及。
    #[serde(default)]
    pub credentials: std::collections::BTreeMap<String, String>,
    /// 任务特征标签，用于分组统计，例如 `no-semantics`、`injection`、`offscreen`、`iframe`。
    #[serde(default)]
    pub tags: Vec<String>,
    pub success: Success,
}

impl Task {
    /// 发给模型的目标描述。契约上 `goal` 本身就是模型输入，因此它必须先通过
    /// [`Task::check_model_facing`]；运行器应只经此方法取 goal，不要直接读字段拼提示词。
    pub fn model_goal(&self) -> &str {
        &self.goal
    }

    /// 模型输入校验（RIG-R01）：`goal` 中不得出现任何秘密值或凭据值。
    /// 凭据只允许被可信执行器按引用名（`credentials` 的键）注入，值永不进入模型输入。
    pub fn check_model_facing(&self) -> anyhow::Result<()> {
        let mut guarded: Vec<String> = self.secrets.clone();
        guarded.extend(self.credentials.values().cloned());
        let leaks = crate::leak::find_leaks(&self.goal, &guarded);
        if !leaks.is_empty() {
            anyhow::bail!(
                "任务 {} 的 goal 包含 {} 个秘密/凭据值，禁止作为模型输入",
                self.id,
                leaks.len()
            );
        }
        Ok(())
    }
}

/// 读取目录下所有 `.toml` 任务（不递归），按文件名排序。
/// 每个任务都会做模型输入校验：goal 含秘密/凭据值的任务文件直接报错，不予加载。
pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<Task>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p)?;
            let task = toml::from_str::<Task>(&text)
                .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
            task.check_model_facing()
                .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
            Ok(task)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn eval_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval")
    }

    #[test]
    fn bundled_web_tasks_parse_with_unique_ids_and_existing_fixtures() {
        let tasks = load_dir(&eval_root().join("tasks/web")).unwrap();
        assert!(tasks.len() >= 5, "至少 5 个内置 Web 任务");

        let mut ids = HashSet::new();
        for t in &tasks {
            assert!(ids.insert(t.id.clone()), "重复的任务 id: {}", t.id);
            assert_eq!(t.scenario, Scenario::Web);
            if let Some(f) = &t.fixture {
                let p = eval_root().join("fixtures/web").join(f);
                assert!(
                    p.exists(),
                    "任务 {} 引用的页面不存在: {}",
                    t.id,
                    p.display()
                );
            }
        }
    }

    #[test]
    fn bundled_tasks_cover_the_hard_cases() {
        let tasks = load_dir(&eval_root().join("tasks/web")).unwrap();
        let tags: HashSet<_> = tasks.iter().flat_map(|t| t.tags.iter().cloned()).collect();
        for needed in ["secret", "offscreen", "injection", "no-semantics", "iframe"] {
            assert!(tags.contains(needed), "缺少覆盖 `{needed}` 的任务");
        }
    }

    #[test]
    fn login_task_keeps_credentials_out_of_the_goal() {
        let tasks = load_dir(&eval_root().join("tasks/web")).unwrap();
        let login = tasks.iter().find(|t| t.id == "web-login-01").unwrap();
        // 凭据以引用存在于可信侧，执行器按名注入；goal 里只有引用名，没有值。
        assert_eq!(
            login.credentials.get("user").map(String::as_str),
            Some("alice")
        );
        assert!(login.credentials.contains_key("pass"));
        assert!(login.check_model_facing().is_ok());
        assert!(!login.model_goal().is_empty());
    }

    #[test]
    fn goal_containing_a_secret_value_is_rejected() {
        let bad = r#"
            id = "x"
            scenario = "web"
            goal = "用密码 hunter2 登录"
            secrets = ["hunter2"]
            [success]
            kind = "manual"
            note = "n"
        "#;
        let task: Task = toml::from_str(bad).unwrap();
        assert!(task.check_model_facing().is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bad = r#"
            id = "x"
            scenario = "web"
            goal = "g"
            sucess = { kind = "manual", note = "typo" }
        "#;
        assert!(toml::from_str::<Task>(bad).is_err());
    }
}
