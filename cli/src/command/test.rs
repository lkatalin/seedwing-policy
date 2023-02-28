use crate::command::verify::Verify;
use crate::Cli;
use seedwing_policy_engine::runtime::{EvalContext, Output, PatternName, RuntimeError, World};
use seedwing_policy_engine::value::RuntimeValue;
use serde_json::Value;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::process::exit;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use walkdir::{DirEntry, WalkDir};

#[derive(clap::Args, Debug)]
#[command(about = "Execute benchmarks", args_conflicts_with_subcommands = true)]
pub struct Test {
    #[arg(short, long = "test", value_name = "DIR")]
    pub(crate) test_directories: Vec<PathBuf>,

    #[arg(short = 'm', long = "match", value_name = "MATCH")]
    pub(crate) r#match: Option<String>,
}

impl Test {
    pub async fn run(&self, args: &Cli) -> Result<(), ()> {
        let world = Verify::verify(args).await?;
        let mut plan = TestPlan::new(&self.test_directories, &self.r#match);
        println!();
        println!("running {} tests", plan.tests.len());
        println!();
        plan.run(&world).await;
        self.display_results(&plan);
        println!();
        let result = if plan.had_failures() { "failed" } else { "ok" };
        println!(
            "test result: {}. {} passed. {} failed. {} pending. {} errors.",
            result,
            plan.passed(),
            plan.failed(),
            plan.pending(),
            plan.error()
        );
        println!();
        if plan.had_failures() {
            exit(-42);
        }
        Ok(())
    }

    pub fn display_results(&self, plan: &TestPlan) {
        let mut last_pattern = None;
        let mut width = 20;
        let mut new_pattern = false;
        for test in &plan.tests {
            if let Some(prev) = &last_pattern {
                if *prev != test.pattern.as_type_str() {
                    new_pattern = true
                }
            } else if last_pattern.is_none() {
                new_pattern = true
            }

            if new_pattern {
                println!("{}", test.pattern.as_type_str());
                width = plan
                    .tests
                    .iter()
                    .filter(|e| e.pattern == test.pattern)
                    .map(|e| e.name.len())
                    .reduce(|accum, e| if e > accum { e } else { accum })
                    .unwrap_or(20)
                    + 3;
            }
            last_pattern.replace(test.pattern.as_type_str());
            new_pattern = false;
            let padding = ".".repeat(width - test.name.len());
            println!(
                "  {}{}{}",
                test.name,
                padding,
                test.result.as_ref().unwrap_or(&TestResult::Pending)
            );
        }
    }
}

#[derive(Debug)]
pub struct TestPlan {
    tests: Vec<TestCase>,
}

impl TestPlan {
    pub fn new(dirs: &[PathBuf], search_pattern: &Option<String>) -> Self {
        let mut tests = dirs
            .iter()
            .flat_map(|dir| {
                WalkDir::new(dir)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                    .flat_map(move |e: DirEntry| {
                        let name = e.file_name().to_string_lossy();
                        if name == "input.json" {
                            if let Ok(path) = e.path().strip_prefix(dir) {
                                let test_name = path.parent().map(|e| e.file_name());
                                let pattern_name = path.parent().map(|e| e.parent());

                                match (pattern_name, test_name) {
                                    (Some(Some(pattern_name)), Some(Some(test_name))) => {
                                        let matchy_name = format!(
                                            "{}::{}",
                                            pattern_name.to_string_lossy().replace('/', "::"),
                                            test_name.to_string_lossy()
                                        );
                                        if !matchy_name.starts_with(
                                            search_pattern.as_ref().unwrap_or(&"".into()),
                                        ) {
                                            return None;
                                        }
                                        let expected = if let Some(parent) = e.path().parent() {
                                            if parent.join("output.json").exists() {
                                                Expected::Transform(parent.join("output.json"))
                                            } else if parent.join("output.identity").exists() {
                                                Expected::Identity
                                            } else if parent.join("output.any").exists() {
                                                Expected::Anything
                                            } else if parent.join("output.none").exists() {
                                                Expected::None
                                            } else {
                                                Expected::Pending
                                            }
                                        } else {
                                            Expected::None
                                        };

                                        Some(TestCase {
                                            name: (*test_name.to_string_lossy()).into(),
                                            pattern: (pattern_name
                                                .to_string_lossy()
                                                .replace('/', "::"))
                                            .into(),
                                            input: e.path().into(),
                                            expected,
                                            result: None,
                                        })
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
            })
            .collect::<Vec<TestCase>>();

        tests.sort_by(|l, r| l.pattern.as_type_str().cmp(&r.pattern.as_type_str()));

        Self { tests }
    }

    pub async fn run(&mut self, world: &World) {
        for test in &mut self.tests.iter_mut() {
            test.run(world).await;
        }
    }

    fn had_failures(&self) -> bool {
        self.tests
            .iter()
            .any(|e| matches!(e.result, Some(TestResult::Error(_) | TestResult::Failed)))
    }

    fn passed(&self) -> usize {
        self.tests
            .iter()
            .flat_map(|e| Some(matches!(e.result, Some(TestResult::Passed))))
            .count()
    }

    fn pending(&self) -> usize {
        self.tests
            .iter()
            .flat_map(|e| Some(matches!(e.result, Some(TestResult::Pending))))
            .count()
    }

    fn error(&self) -> usize {
        self.tests
            .iter()
            .flat_map(|e| Some(matches!(e.result, Some(TestResult::Error(_)))))
            .count()
    }

    fn failed(&self) -> usize {
        self.tests
            .iter()
            .flat_map(|e| Some(matches!(e.result, Some(TestResult::Failed))))
            .count()
    }
}

#[derive(Debug)]
pub struct TestCase {
    name: String,
    pattern: PatternName,
    input: PathBuf,
    expected: Expected,
    result: Option<TestResult>,
}

impl TestCase {
    pub async fn run(&mut self, world: &World) {
        if let Expected::Pending = &self.expected {
            self.result.replace(TestResult::Pending);
            return;
        }

        if let Ok(mut input_file) = File::open(&self.input).await {
            let mut input = Vec::new();
            let read_result = input_file.read_to_end(&mut input).await;
            if read_result.is_ok() {
                let input: Result<Value, _> = serde_json::from_slice(&input);
                if let Ok(input) = input {
                    let result = world
                        .evaluate(self.pattern.as_type_str(), input, EvalContext::default())
                        .await;

                    match result {
                        Ok(result) => match (result.raw_output(), &self.expected) {
                            (Output::None, Expected::None) => {
                                self.result.replace(TestResult::Passed);
                            }
                            (Output::Identity, Expected::Identity) => {
                                self.result.replace(TestResult::Passed);
                            }
                            (Output::Identity, Expected::Anything) => {
                                self.result.replace(TestResult::Passed);
                            }
                            (Output::Transform(val), Expected::Transform(expected_val)) => {
                                if let Ok(mut output_file) = File::open(expected_val).await {
                                    let mut output = Vec::new();
                                    let read_result = output_file.read_to_end(&mut output).await;
                                    if read_result.is_ok() {
                                        let output: Result<Value, _> =
                                            serde_json::from_slice(&output);
                                        if let Ok(output) = output {
                                            let output: RuntimeValue = output.into();

                                            if *val.as_ref() == output {
                                                self.result.replace(TestResult::Passed);
                                            }
                                        }
                                    }
                                }
                                if self.result.is_none() {
                                    self.result.replace(TestResult::Failed);
                                }
                            }
                            (Output::Transform(_val), Expected::Anything) => {
                                self.result.replace(TestResult::Passed);
                            }
                            _ => {
                                self.result.replace(TestResult::Failed);
                            }
                        },
                        Err(err) => {
                            self.result
                                .replace(TestResult::Error(TestError::Runtime(err)));
                        }
                    }
                } else {
                    self.result
                        .replace(TestResult::Error(TestError::Deserialization));
                }
            } else {
                self.result
                    .replace(TestResult::Error(TestError::ReadingInput));
            }
        } else {
            self.result
                .replace(TestResult::Error(TestError::ReadingInput));
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expected {
    Pending,
    Anything,
    Identity,
    Transform(PathBuf),
    None,
}

#[derive(Debug)]
pub enum TestResult {
    Pending,
    Passed,
    Failed,
    Error(TestError),
}

impl Display for TestResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TestResult::Passed => write!(f, "passed"),
            TestResult::Failed => write!(f, "failed"),
            TestResult::Pending => write!(f, "pending"),
            TestResult::Error(err) => write!(f, "error: {:?}", err),
        }
    }
}

#[derive(Debug)]
pub enum TestError {
    ReadingInput,
    Deserialization,
    Runtime(RuntimeError),
}
