/// File type definitions for `--type` / `-t` filtering (ripgrep-compatible).
use std::collections::HashMap;
use std::sync::LazyLock;

/// Built-in file type definitions mapping type names to glob patterns.
/// Initialized once and cached for the lifetime of the process.
static BUILTIN_TYPES: LazyLock<HashMap<&'static str, &'static [&'static str]>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("asm", &["*.asm", "*.s", "*.S"][..]);
        m.insert("avro", &["*.avdl", "*.avpr", "*.avsc"]);
        m.insert("bazel", &["*.bzl", "*.bazel", "BUILD", "WORKSPACE"]);
        m.insert(
            "bitbake",
            &["*.bb", "*.bbappend", "*.bbclass", "*.conf", "*.inc"],
        );
        m.insert("c", &["*.c", "*.h", "*.H"]);
        m.insert("clojure", &["*.clj", "*.cljc", "*.cljs", "*.edn"]);
        m.insert("cmake", &["*.cmake", "CMakeLists.txt"]);
        m.insert("coffeescript", &["*.coffee"]);
        m.insert("config", &["*.cfg", "*.conf", "*.config", "*.ini"]);
        m.insert(
            "cpp",
            &[
                "*.C", "*.cc", "*.cpp", "*.cxx", "*.h", "*.H", "*.hh", "*.hpp", "*.hxx", "*.inl",
            ],
        );
        m.insert("cs", &["*.cs"]);
        m.insert("csharp", &["*.cs"]);
        m.insert("csproj", &["*.csproj"]);
        m.insert("css", &["*.css", "*.scss", "*.less"]);
        m.insert("csv", &["*.csv"]);
        m.insert("cuda", &["*.cu", "*.cuh"]);
        m.insert("d", &["*.d"]);
        m.insert("dart", &["*.dart"]);
        m.insert("diff", &["*.diff", "*.patch"]);
        m.insert("docker", &["Dockerfile", "*.dockerfile"]);
        m.insert("elixir", &["*.ex", "*.exs"]);
        m.insert("elm", &["*.elm"]);
        m.insert("erb", &["*.erb"]);
        m.insert("erlang", &["*.erl", "*.hrl"]);
        m.insert("fish", &["*.fish"]);
        m.insert(
            "fortran",
            &["*.f", "*.f77", "*.f90", "*.f95", "*.for", "*.fpp"],
        );
        m.insert("fsharp", &["*.fs", "*.fsi", "*.fsx"]);
        m.insert("go", &["*.go"]);
        m.insert("graphql", &["*.gql", "*.graphql"]);
        m.insert("groovy", &["*.gradle", "*.groovy"]);
        m.insert("h", &["*.h", "*.hpp"]);
        m.insert("haskell", &["*.hs", "*.lhs"]);
        m.insert("html", &["*.htm", "*.html", "*.xhtml"]);
        m.insert("java", &["*.java"]);
        m.insert("js", &["*.js", "*.jsx", "*.mjs", "*.cjs"]);
        m.insert("json", &["*.json", "*.jsonl"]);
        m.insert("jsonl", &["*.jsonl"]);
        m.insert("julia", &["*.jl"]);
        m.insert("kotlin", &["*.kt", "*.kts"]);
        m.insert("less", &["*.less"]);
        m.insert("lisp", &["*.el", "*.lisp", "*.lsp", "*.cl"]);
        m.insert("lock", &["*.lock"]);
        m.insert("log", &["*.log"]);
        m.insert("lua", &["*.lua"]);
        m.insert("make", &["Makefile", "*.mk", "*.mak", "GNUmakefile"]);
        m.insert("markdown", &["*.md", "*.markdown", "*.mkd"]);
        m.insert("md", &["*.md", "*.markdown", "*.mkd"]);
        m.insert("nim", &["*.nim"]);
        m.insert("nix", &["*.nix"]);
        m.insert("objc", &["*.m", "*.h"]);
        m.insert("objcpp", &["*.mm", "*.h"]);
        m.insert("ocaml", &["*.ml", "*.mli"]);
        m.insert("org", &["*.org"]);
        m.insert("pascal", &["*.pas", "*.pp"]);
        m.insert("perl", &["*.pl", "*.pm", "*.t"]);
        m.insert("php", &["*.php", "*.phtml"]);
        m.insert("protobuf", &["*.proto"]);
        m.insert("py", &["*.py", "*.pyi"]);
        m.insert("python", &["*.py", "*.pyi"]);
        m.insert("qml", &["*.qml"]);
        m.insert("r", &["*.r", "*.R", "*.Rmd"]);
        m.insert("readme", &["README", "README.*"]);
        m.insert("ruby", &["*.rb", "*.gemspec", "Gemfile", "Rakefile"]);
        m.insert("rust", &["*.rs"]);
        m.insert("sass", &["*.sass", "*.scss"]);
        m.insert("scala", &["*.scala", "*.sbt"]);
        m.insert("shell", &["*.bash", "*.sh", "*.zsh"]);
        m.insert("sh", &["*.bash", "*.sh", "*.zsh"]);
        m.insert("sql", &["*.sql"]);
        m.insert("swift", &["*.swift"]);
        m.insert("tex", &["*.tex", "*.ltx", "*.cls", "*.sty"]);
        m.insert("tf", &["*.tf", "*.tfvars"]);
        m.insert("toml", &["*.toml"]);
        m.insert("ts", &["*.ts", "*.tsx", "*.mts", "*.cts"]);
        m.insert("typescript", &["*.ts", "*.tsx", "*.mts", "*.cts"]);
        m.insert("txt", &["*.txt"]);
        m.insert("vim", &["*.vim"]);
        m.insert("xml", &["*.xml", "*.xsl", "*.xslt", "*.xsd", "*.svg"]);
        m.insert("yaml", &["*.yml", "*.yaml"]);
        m.insert("zig", &["*.zig"]);
        m
    });

/// Return the cached built-in file type definitions.
pub fn builtin_types() -> &'static HashMap<&'static str, &'static [&'static str]> {
    &BUILTIN_TYPES
}

/// Check if a file path matches any of the glob patterns for a type.
pub fn matches_type(path: &str, type_name: &str) -> bool {
    let types = builtin_types();
    let patterns = match types.get(type_name) {
        Some(p) => *p,
        None => return false,
    };

    let filename = path.rsplit('/').next().unwrap_or(path);
    let filename_win = filename.rsplit('\\').next().unwrap_or(filename);
    let filename = if filename_win.len() < filename.len() {
        filename_win
    } else {
        filename
    };

    for pattern in patterns {
        if pattern.starts_with("*.") {
            let ext = &pattern[1..]; // ".rs", ".py", etc.
            if filename.ends_with(ext) {
                return true;
            }
        } else {
            // Exact filename match (e.g., "Makefile", "Dockerfile")
            if filename == *pattern {
                return true;
            }
        }
    }
    false
}

/// Print all known file types and their associated globs.
pub fn print_type_list() {
    let types = builtin_types();
    let mut names: Vec<&&str> = types.keys().collect();
    names.sort();
    for name in names {
        let globs = types[name];
        println!("{}: {}", name, globs.join(", "));
    }
}
