// SPDX-License-Identifier: Apache-2.0

//! This `build.rs` generates code for the local semantic convention registry located in the
//! `semconv_registry` directory. The templates used for code generation are located in the
//! `templates/registry/rust` directory. The generated code is produced in the `OUT_DIR` directory
//! specified by Cargo. This generation step occurs before the standard build of the crate. The
//! generated code, along with the standard crate code, will be compiled together.

use log::error;
use miette::Diagnostic;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::exit;
use weaver_common::vdir::VirtualDirectoryPath;
use weaver_common::MemLog;
use weaver_forge::config::{Params, WeaverConfig};
use weaver_forge::file_loader::FileSystemFileLoader;
use weaver_forge::registry::ResolvedRegistry;
use weaver_forge::{OutputDirective, TemplateEngine};
use weaver_resolver::SchemaResolver;
use weaver_semconv::registry::SemConvRegistry;
use weaver_semconv::registry_repo::RegistryRepo;

const SEMCONV_REGISTRY_PATH: &str = "./semconv_registry/";
const TEMPLATES_PATH: &str = "./templates/registry/";
const TARGET: &str = "rust";
const FOLLOW_SYMLINKS: bool = false;

fn main() {
    // Tell Cargo when to rerun this build script
    println!("cargo:rerun-if-changed=templates/registry/rust");
    println!("cargo:rerun-if-changed=semconv_registry");
    println!("cargo:rerun-if-changed=build.rs");

    // Get the output directory from Cargo
    let target_dir = std::env::var("OUT_DIR").expect("Failed to get OUT_DIR from Cargo");

    // Create an in-memory logger as stdout and stderr are not "classically" available in build.rs.
    let logger = MemLog::new();

    // Load and resolve the semantic convention registry
    let registry_path = VirtualDirectoryPath::LocalFolder {
        path: SEMCONV_REGISTRY_PATH.into(),
    };
    let registry_repo =
        RegistryRepo::try_new("main", &registry_path).unwrap_or_else(|e| process_error(&logger, e));
    let semconv_specs = SchemaResolver::load_semconv_specs(&registry_repo, true, FOLLOW_SYMLINKS)
        .ignore(|e| matches!(e.severity(), Some(miette::Severity::Warning)))
        .into_result_failing_non_fatal()
        .unwrap_or_else(|e| process_error(&logger, e));
    let mut registry = SemConvRegistry::from_semconv_specs(&registry_repo, semconv_specs)
        .unwrap_or_else(|e| process_error(&logger, e));
    let schema = SchemaResolver::resolve_semantic_convention_registry(&mut registry, false)
        .into_result_failing_non_fatal()
        .unwrap_or_else(|e| process_error(&logger, e));

    let loader = FileSystemFileLoader::try_new(TEMPLATES_PATH.into(), TARGET)
        .unwrap_or_else(|e| process_error(&logger, e));
    let config = WeaverConfig::try_from_path("./templates/registry/rust")
        .unwrap_or_else(|e| process_error(&logger, e));
    let engine = TemplateEngine::new(config, loader, Params::default());
    let template_registry =
        ResolvedRegistry::try_from_resolved_registry(&schema.registry, schema.catalog())
            .unwrap_or_else(|e| process_error(&logger, e));
    let target_dir: PathBuf = target_dir.into();
    engine
        .generate(
            &template_registry,
            target_dir.as_path(),
            &OutputDirective::File,
        )
        .unwrap_or_else(|e| process_error(&logger, e));

    print_logs(&logger);

    // For the purpose of the integration test located in `tests/codegen.rs`, we need to:
    // - Combine all generated files to form a single file containing all the generated code
    // organized in nested modules.
    // - Replace `//!` with `//`
    // - Remove `pub mod` lines
    create_single_generated_rs_file(target_dir.as_path());
}

/// Create a single generated.rs file containing all the generated code organized in nested modules.
fn create_single_generated_rs_file(root: &Path) {
    let mut root_module = Module {
        name: "".to_owned(),
        content: "".to_owned(),
        sub_modules: HashMap::new(),
    };

    // Traverse the directory and add the content of each file to the root module
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.expect("Failed to read entry");
        let path = entry.path();

        // Only process files with the .rs extension that contain the generated comment
        if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            let file_content = std::fs::read_to_string(path).expect("Failed to read file");

            // Only process files that have been generated
            // This test expects the comment `DO NOT EDIT, THIS FILE HAS BEEN GENERATED BY WEAVER`
            // to be present in each generated file.
            if file_content.contains("DO NOT EDIT, THIS FILE HAS BEEN GENERATED BY WEAVER") {
                let relative_path = path.strip_prefix(root).expect("Failed to strip prefix");
                let file_name = path
                    .file_stem()
                    .expect("Failed to extract the file name")
                    .to_str()
                    .expect("Failed to convert to string");
                let parent_modules = relative_path.parent().map_or(vec![], |parent| {
                    parent
                        .components()
                        .filter_map(|component| match component {
                            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                            _ => None,
                        })
                        .collect()
                });

                // Skip generated.rs
                if file_name == "generated" {
                    continue;
                }

                // Replace //! with //
                // Nested modules doesn't support //! comments
                let new_file_content = file_content.replace("//!", "//");

                // Remove lines starting with `pub mod` because we are going to nest the modules
                // manually in the generated.rs file
                let new_file = new_file_content
                    .lines()
                    .filter(|line| !line.starts_with("pub mod"))
                    .collect::<Vec<_>>()
                    .join("\n");

                add_modules(
                    &mut root_module,
                    parent_modules.clone(),
                    file_name.to_owned(),
                    new_file,
                );
            }
        }
    }

    // Generate `generated.rs` from the hierarchy of modules
    let mut output =
        std::fs::File::create(root.join("generated.rs")).expect("Failed to create file");
    output
        .write_all(root_module.generate().as_bytes())
        .expect("Failed to write to file");
}

struct Module {
    name: String,
    content: String,
    sub_modules: HashMap<String, Module>,
}

impl Module {
    /// Generate the content of the module and its sub-modules
    pub fn generate(&self) -> String {
        let mut content = String::new();

        content.push_str(&self.content);

        for module in self.sub_modules.values() {
            content.push_str("/// Generated module");
            content.push_str(&format!("\npub mod {} {{\n", module.name));
            content.push_str(&module.generate());
            content.push_str("\n}\n");
        }
        content
    }
}

/// Add the given module to the hierarchy of modules represented by the root module.
fn add_modules(
    root_module: &mut Module,
    parent_modules: Vec<String>,
    module_name: String,
    module_content: String,
) {
    let mut current_module = root_module;
    for module_name in parent_modules.iter() {
        let module = current_module
            .sub_modules
            .entry(module_name.clone())
            .or_insert(Module {
                name: module_name.clone(),
                content: "".to_owned(),
                sub_modules: HashMap::new(),
            });
        current_module = module;
    }

    if module_name == "mod" || module_name == "lib" {
        current_module.content = module_content;
    } else {
        _ = current_module.sub_modules.insert(
            module_name.clone(),
            Module {
                name: module_name.clone(),
                content: module_content,
                sub_modules: HashMap::new(),
            },
        );
    }
}

/// Print logs to stdout by following the Cargo's build script output format.
fn print_logs(logger: &MemLog) {
    for record in logger.records() {
        match &record.level {
            log::Level::Warn => println!("cargo:warning={}", record.message),
            log::Level::Error => println!("cargo:warning=Error: {}", record.message),
            _ => { /* Ignore */ }
        }
    }
}

/// Process the error message and exit the build script with a non-zero exit code.
fn process_error(logger: &MemLog, error: impl std::fmt::Display) -> ! {
    error!("{}", &error.to_string());
    print_logs(logger);
    #[allow(clippy::exit)] // Expected behavior
    exit(1)
}
