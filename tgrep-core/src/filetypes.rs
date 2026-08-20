//! File type definitions for `--type`/`-t` filtering.
//!
//! `DEFAULT_TYPES` is a verbatim copy of ripgrep's
//! `crates/ignore/src/default_types.rs` table, deliberately kept in its
//! upstream shape (including the `(&[names], &[globs])` tuple form and the
//! alias lists) so it can be re-synced by replacing the table wholesale.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use anyhow::{Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Every type name and alias mapped to its glob patterns.
pub type TypeMap = BTreeMap<&'static str, &'static [&'static str]>;

static BUILTIN_TYPES: LazyLock<TypeMap> = LazyLock::new(|| {
    let mut m = BTreeMap::new();
    for (names, globs) in DEFAULT_TYPES {
        for name in *names {
            m.insert(*name, *globs);
        }
    }
    m
});

/// Return the built-in type table, keyed by every name *and* alias.
pub fn builtin_types() -> &'static TypeMap {
    &BUILTIN_TYPES
}

/// The type definitions in effect for one invocation: the built-ins plus any
/// `--type-add`, minus any `--type-clear`.
#[derive(Clone, Debug)]
pub struct TypeDefs {
    map: BTreeMap<String, Vec<String>>,
}

impl Default for TypeDefs {
    fn default() -> Self {
        Self::builtin()
    }
}

impl TypeDefs {
    /// Start from ripgrep's built-in table.
    pub fn builtin() -> Self {
        let map = builtin_types()
            .iter()
            .map(|(name, globs)| {
                (
                    (*name).to_string(),
                    globs.iter().map(|g| (*g).to_string()).collect(),
                )
            })
            .collect();
        Self { map }
    }

    /// Apply `--type-clear NAME`.
    pub fn clear(&mut self, name: &str) {
        self.map.remove(name);
    }

    /// Apply `--type-add SPEC`, where SPEC is `name:glob` or
    /// `name:include:type1,type2`.
    pub fn add(&mut self, spec: &str) -> Result<()> {
        let Some((name, rest)) = spec.split_once(':') else {
            bail!("invalid --type-add value `{spec}`: expected `name:glob`");
        };
        if name.is_empty() || rest.is_empty() {
            bail!("invalid --type-add value `{spec}`: name and definition must be non-empty");
        }
        if name.contains(':') {
            bail!("invalid --type-add value `{spec}`: type name may not contain `:`");
        }
        if let Some(list) = rest.strip_prefix("include:") {
            // `include:` copies the globs of other types *at the time the flag
            // is processed*, so ordering between --type-add flags matters, the
            // same way it does in ripgrep.
            let mut inherited = Vec::new();
            for dep in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match self.map.get(dep) {
                    Some(globs) => inherited.extend(globs.iter().cloned()),
                    None => {
                        bail!("invalid --type-add value `{spec}`: unrecognized file type `{dep}`")
                    }
                }
            }
            if inherited.is_empty() {
                bail!("invalid --type-add value `{spec}`: `include:` needs at least one type");
            }
            self.map
                .entry(name.to_string())
                .or_default()
                .extend(inherited);
        } else {
            self.map
                .entry(name.to_string())
                .or_default()
                .push(rest.to_string());
        }
        Ok(())
    }

    /// Whether a type name is defined.
    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// Glob patterns for one type, if defined.
    pub fn globs(&self, name: &str) -> Option<&[String]> {
        self.map.get(name).map(|v| v.as_slice())
    }

    /// Print every definition, one per line, like `rg --type-list`.
    pub fn print_list(&self) {
        for (name, globs) in &self.map {
            println!("{}: {}", name, globs.join(", "));
        }
    }

    /// Compile `-t`/`-T` selections into a matcher.
    pub fn build_filter(&self, select: &[String], negate: &[String]) -> Result<TypeFilter> {
        Ok(TypeFilter {
            select: self.compile(select)?,
            negate: self.compile(negate)?,
        })
    }

    fn compile(&self, names: &[String]) -> Result<Option<GlobSet>> {
        if names.is_empty() {
            return Ok(None);
        }
        let mut builder = GlobSetBuilder::new();
        for name in names {
            let Some(globs) = self.map.get(name.as_str()) else {
                bail!("unrecognized file type: {name}");
            };
            for glob in globs {
                // ripgrep matches type globs against the file name alone, so a
                // `*` must not be allowed to cross a path separator.
                builder.add(GlobBuilder::new(glob).literal_separator(true).build()?);
            }
        }
        Ok(Some(builder.build()?))
    }
}

/// Compiled `-t`/`-T` filter.
///
/// `-T` takes precedence over `-t`, and when only `-T` is given every file that
/// is not excluded is included.
#[derive(Debug, Default)]
pub struct TypeFilter {
    select: Option<GlobSet>,
    negate: Option<GlobSet>,
}

impl TypeFilter {
    /// True when no `-t`/`-T` was given, so the filter passes everything.
    pub fn is_empty(&self) -> bool {
        self.select.is_none() && self.negate.is_none()
    }

    /// Whether `path` survives the filter.
    pub fn matches(&self, path: &str) -> bool {
        if self.is_empty() {
            return true;
        }
        let name = base_name(path);
        if let Some(negate) = &self.negate
            && negate.is_match(name)
        {
            return false;
        }
        match &self.select {
            Some(select) => select.is_match(name),
            None => true,
        }
    }
}

/// Last path component, handling both separators so Windows paths that were
/// rendered with forward slashes still resolve correctly.
fn base_name(path: &str) -> &str {
    let after_slash = path.rsplit('/').next().unwrap_or(path);
    after_slash.rsplit('\\').next().unwrap_or(after_slash)
}

#[rustfmt::skip]
pub const DEFAULT_TYPES: &[(&[&str], &[&str])] = &[
    (&["ada"], &["*.adb", "*.ads"]),
    (&["agda"], &["*.agda", "*.lagda"]),
    (&["aidl"], &["*.aidl"]),
    (&["alire"], &["alire.toml"]),
    (&["amake"], &["*.mk", "*.bp"]),
    (&["asciidoc"], &["*.adoc", "*.asc", "*.asciidoc"]),
    (&["asm"], &["*.asm", "*.s", "*.S"]),
    (&["asp"], &[
        "*.aspx", "*.aspx.cs", "*.aspx.vb", "*.ascx", "*.ascx.cs",
        "*.ascx.vb", "*.asp"
    ]),
    (&["ats"], &["*.ats", "*.dats", "*.sats", "*.hats"]),
    (&["avro"], &["*.avdl", "*.avpr", "*.avsc"]),
    (&["awk"], &["*.awk"]),
    (&["bat", "batch"], &["*.bat"]),
    (&["bazel"], &[
        "*.bazel", "*.bzl", "*.BUILD", "*.bazelrc", "BUILD", "MODULE.bazel",
        "WORKSPACE", "WORKSPACE.bazel", "WORKSPACE.bzlmod",
    ]),
    (&["bitbake"], &["*.bb", "*.bbappend", "*.bbclass", "*.conf", "*.inc"]),
    (&["boxlang"], &["*.bx", "*.bxm", "*.bxs"]),
    (&["brotli"], &["*.br"]),
    (&["buildstream"], &["*.bst"]),
    (&["bzip2"], &["*.bz2", "*.tbz2"]),
    (&["c"], &["*.[chH]", "*.[chH].in", "*.cats"]),
    (&["cabal"], &["*.cabal"]),
    (&["candid"], &["*.did"]),
    (&["carp"], &["*.carp"]),
    (&["cbor"], &["*.cbor"]),
    (&["ceylon"], &["*.ceylon"]),
    (&["cfml"], &["*.cfc", "*.cfm"]),
    (&["clojure"], &["*.clj", "*.cljc", "*.cljs", "*.cljx"]),
    (&["cmake"], &["*.cmake", "CMakeLists.txt"]),
    (&["cmd"], &["*.bat", "*.cmd"]),
    (&["cml"], &["*.cml"]),
    (&["coffeescript"], &["*.coffee"]),
    (&["config"], &["*.cfg", "*.conf", "*.config", "*.ini"]),
    (&["container"], &["*Containerfile*", "*Dockerfile*"]),
    (&["coq"], &["*.v"]),
    (&["cpp"], &[
        "*.[ChH]", "*.cc", "*.[ch]pp", "*.[ch]xx", "*.hh",  "*.inl",
        "*.[ChH].in", "*.cc.in", "*.[ch]pp.in", "*.[ch]xx.in", "*.hh.in",
    ]),
    (&["creole"], &["*.creole"]),
    (&["crystal"], &["Projectfile", "*.cr", "*.ecr", "shard.yml"]),
    (&["cs"], &["*.cs"]),
    (&["csharp"], &["*.cs"]),
    (&["cshtml"], &["*.cshtml"]),
    (&["csproj"], &["*.csproj"]),
    (&["css"], &["*.css", "*.scss"]),
    (&["csv"], &["*.csv"]),
    (&["cuda"], &["*.cu", "*.cuh"]),
    (&["cython"], &["*.pyx", "*.pxi", "*.pxd"]),
    (&["d"], &["*.d"]),
    (&["dart"], &["*.dart"]),
    (&["devicetree"], &["*.dts", "*.dtsi", "*.dtso"]),
    (&["dhall"], &["*.dhall"]),
    (&["diff"], &["*.patch", "*.diff"]),
    (&["dita"], &["*.dita", "*.ditamap", "*.ditaval"]),
    (&["docker"], &["*Dockerfile*"]),
    (&["dockercompose"], &["docker-compose.yml", "docker-compose.*.yml"]),
    (&["dts"], &["*.dts", "*.dtsi"]),
    (&["dvc"], &["Dvcfile", "*.dvc"]),
    (&["ebuild"], &["*.ebuild", "*.eclass"]),
    (&["edn"], &["*.edn"]),
    (&["elisp"], &["*.el"]),
    (&["elixir"], &["*.ex", "*.eex", "*.exs", "*.heex", "*.leex", "*.livemd"]),
    (&["elm"], &["*.elm"]),
    (&["erb"], &["*.erb"]),
    (&["erlang"], &["*.erl", "*.hrl"]),
    (&["fennel"], &["*.fnl"]),
    (&["fidl"], &["*.fidl"]),
    (&["fish"], &["*.fish"]),
    (&["flatbuffers"], &["*.fbs"]),
    (&["fortran"], &[
        "*.f", "*.F", "*.f77", "*.F77", "*.pfo",
        "*.f90", "*.F90", "*.f95", "*.F95",
    ]),
    (&["fsharp"], &["*.fs", "*.fsx", "*.fsi"]),
    (&["fut"], &["*.fut"]),
    (&["gap"], &["*.g", "*.gap", "*.gi", "*.gd", "*.tst"]),
    (&["gdscript"], &["*.gd"]),
    (&["gleam"], &["*.gleam"]),
    (&["gn"], &["*.gn", "*.gni"]),
    (&["go"], &["*.go"]),
    (&["gprbuild"], &["*.gpr"]),
    (&["gradle"], &[
        "*.gradle", "*.gradle.kts", "gradle.properties", "gradle-wrapper.*",
        "gradlew", "gradlew.bat",
    ]),
    (&["graphql"], &["*.graphql", "*.graphqls"]),
    (&["groovy"], &["*.groovy", "*.gradle"]),
    (&["gzip"], &["*.gz", "*.tgz"]),
    (&["h"], &["*.h", "*.hh", "*.hpp"]),
    (&["haml"], &["*.haml"]),
    (&["hare"], &["*.ha"]),
    (&["haskell"], &["*.hs", "*.lhs", "*.cpphs", "*.c2hs", "*.hsc"]),
    (&["hbs"], &["*.hbs"]),
    (&["hs"], &["*.hs", "*.lhs"]),
    (&["html"], &["*.htm", "*.html", "*.ejs"]),
    (&["hurl"], &["*.hurl"]),
    (&["hy"], &["*.hy"]),
    (&["idris"], &["*.idr", "*.lidr"]),
    (&["janet"], &["*.janet"]),
    (&["java"], &["*.java", "*.jsp", "*.jspx", "*.properties"]),
    (&["jinja"], &["*.j2", "*.jinja", "*.jinja2"]),
    (&["jl"], &["*.jl"]),
    (&["js"], &["*.js", "*.jsx", "*.vue", "*.cjs", "*.mjs"]),
    (&["json"], &["*.json", "composer.lock", "*.sarif"]),
    (&["jsonl"], &["*.jsonl"]),
    (&["julia"], &["*.jl"]),
    (&["jupyter"], &["*.ipynb", "*.jpynb"]),
    (&["k"], &["*.k"]),
    (&["kconfig"], &["Kconfig", "Kconfig.*"]),
    (&["kotlin"], &["*.kt", "*.kts"]),
    (&["lean"], &["*.lean"]),
    (&["less"], &["*.less"]),
    (&["license"], &[
        // General
        "COPYING", "COPYING[.-]*",
        "COPYRIGHT", "COPYRIGHT[.-]*",
        "EULA", "EULA[.-]*",
        "licen[cs]e", "licen[cs]e.*",
        "LICEN[CS]E", "LICEN[CS]E[.-]*", "*[.-]LICEN[CS]E*",
        "NOTICE", "NOTICE[.-]*",
        "PATENTS", "PATENTS[.-]*",
        "UNLICEN[CS]E", "UNLICEN[CS]E[.-]*",
        // GPL (gpl.txt, etc.)
        "agpl[.-]*",
        "gpl[.-]*",
        "lgpl[.-]*",
        // Other license-specific (APACHE-2.0.txt, etc.)
        "AGPL-*[0-9]*",
        "APACHE-*[0-9]*",
        "BSD-*[0-9]*",
        "CC-BY-*",
        "GFDL-*[0-9]*",
        "GNU-*[0-9]*",
        "GPL-*[0-9]*",
        "LGPL-*[0-9]*",
        "MIT-*[0-9]*",
        "MPL-*[0-9]*",
        "OFL-*[0-9]*",
    ]),
    (&["lilypond"], &["*.ly", "*.ily"]),
    (&["lisp"], &["*.el", "*.jl", "*.lisp", "*.lsp", "*.sc", "*.scm"]),
    (&["llvm"], &["*.ll"]),
    (&["lock"], &["*.lock", "package-lock.json"]),
    (&["log"], &["*.log"]),
    (&["lua"], &["*.lua"]),
    (&["lz4"], &["*.lz4"]),
    (&["lzma"], &["*.lzma"]),
    (&["m4"], &["*.ac", "*.m4"]),
    (&["make"], &[
        "[Gg][Nn][Uu]makefile", "[Mm]akefile",
        "[Gg][Nn][Uu]makefile.am", "[Mm]akefile.am",
        "[Gg][Nn][Uu]makefile.in", "[Mm]akefile.in",
        "Makefile.*",
        "*.mk", "*.mak"
    ]),
    (&["mako"], &["*.mako", "*.mao"]),
    (&["man"], &["*.[0-9lnpx]", "*.[0-9][cEFMmpSx]"]),
    (&["markdown", "md"], &[
        "*.markdown",
        "*.md",
        "*.mdown",
        "*.mdwn",
        "*.mkd",
        "*.mkdn",
        "*.mdx",
    ]),
    (&["matlab"], &["*.m"]),
    (&["meson"], &["meson.build", "meson_options.txt", "meson.options"]),
    (&["minified"], &["*.min.html", "*.min.css", "*.min.js"]),
    (&["mint"], &["*.mint"]),
    (&["mk"], &["mkfile"]),
    (&["ml"], &["*.ml"]),
    (&["mojo"], &["*.mojo"]),
    (&["motoko"], &["*.mo"]),
    (&["msbuild"], &[
        "*.csproj", "*.fsproj", "*.vcxproj", "*.proj", "*.props", "*.targets",
        "*.sln", "*.slnf"
    ]),
    (&["nim"], &["*.nim", "*.nimf", "*.nimble", "*.nims"]),
    (&["nix"], &["*.nix"]),
    (&["objc"], &["*.h", "*.m"]),
    (&["objcpp"], &["*.h", "*.mm"]),
    (&["ocaml"], &["*.ml", "*.mli", "*.mll", "*.mly"]),
    (&["org"], &["*.org", "*.org_archive"]),
    (&["pants"], &["BUILD"]),
    (&["pascal"], &["*.pas", "*.dpr", "*.lpr", "*.pp", "*.inc"]),
    (&["pdf"], &["*.pdf"]),
    (&["perl"], &["*.perl", "*.pl", "*.PL", "*.plh", "*.plx", "*.pm", "*.t"]),
    (&["php"], &[
        // note that PHP 6 doesn't exist
        // See: https://wiki.php.net/rfc/php6
        "*.php", "*.php3", "*.php4", "*.php5", "*.php7", "*.php8",
        "*.pht", "*.phtml"
    ]),
    (&["pkgbuild"], &["PKGBUILD"]),
    (&["po"], &["*.po"]),
    (&["pod"], &["*.pod"]),
    (&["postscript"], &["*.eps", "*.ps"]),
    (&["prolog"], &["*.pl", "*.pro", "*.prolog", "*.P"]),
    (&["proto", "protobuf"], &["*.proto"]),
    (&["ps"], &["*.cdxml", "*.ps1", "*.ps1xml", "*.psd1", "*.psm1"]),
    (&["puppet"], &["*.epp", "*.erb", "*.pp", "*.rb"]),
    (&["purs"], &["*.purs"]),
    (&["py", "python"], &["*.py", "*.pyi"]),
    (&["qmake"], &["*.pro", "*.pri", "*.prf"]),
    (&["qml"], &["*.qml"]),
    (&["qrc"], &["*.qrc"]),
    (&["qui"], &["*.ui"]),
    (&["r"], &["*.R", "*.r", "*.Rmd", "*.rmd", "*.Rnw", "*.rnw"]),
    (&["racket"], &["*.rkt"]),
    (&["raku"], &[
        "*.raku", "*.rakumod", "*.rakudoc", "*.rakutest",
        "*.p6", "*.pl6", "*.pm6"
    ]),
    (&["rdoc"], &["*.rdoc"]),
    (&["readme"], &["README*", "*README"]),
    (&["reasonml"], &["*.re", "*.rei"]),
    (&["red"], &["*.r", "*.red", "*.reds"]),
    (&["rescript"], &["*.res", "*.resi"]),
    (&["robot"], &["*.robot"]),
    (&["rocq"], &["*.v"]),
    (&["rst"], &["*.rst"]),
    (&["ruby"], &[
        // Idiomatic files
        "config.ru", "Gemfile", ".irbrc", "Rakefile",
        // Extensions
        "*.gemspec", "*.rb", "*.rbw", "*.rake"
    ]),
    (&["rust"], &["*.rs"]),
    (&["sass"], &["*.sass", "*.scss"]),
    (&["scala"], &["*.scala", "*.sbt"]),
    (&["scdoc"], &["*.scd", "*.scdoc"]),
    (&["seed7"], &["*.sd7", "*.s7i"]),
    (&["sh"], &[
        // Portable/misc. init files
        ".env", ".login", ".logout", ".profile", "profile",
        // bash-specific init files
        ".bash_login", "bash_login",
        ".bash_logout", "bash_logout",
        ".bash_profile", "bash_profile",
        ".bashrc", "bashrc", "*.bashrc",
        // csh-specific init files
        ".cshrc", "*.cshrc",
        // ksh-specific init files
        ".kshrc", "*.kshrc",
        // tcsh-specific init files
        ".tcshrc",
        // zsh-specific init files
        ".zshenv", "zshenv",
        ".zlogin", "zlogin",
        ".zlogout", "zlogout",
        ".zprofile", "zprofile",
        ".zshrc", "zshrc",
        // Extensions
        "*.bash", "*.csh", "*.env", "*.ksh", "*.sh", "*.tcsh", "*.zsh",
    ]),
    (&["slim"], &["*.skim", "*.slim", "*.slime"]),
    (&["smarty"], &["*.tpl"]),
    (&["sml"], &["*.sml", "*.sig"]),
    (&["solidity"], &["*.sol"]),
    (&["soy"], &["*.soy"]),
    (&["spark"], &["*.spark"]),
    (&["spec"], &["*.spec"]),
    (&["sql"], &["*.sql", "*.psql"]),
    (&["ssa"], &["*.ssa"]),
    (&["stylus"], &["*.styl"]),
    (&["sv"], &["*.v", "*.vg", "*.sv", "*.svh", "*.h"]),
    (&["svelte"], &["*.svelte", "*.svelte.ts"]),
    (&["svg"], &["*.svg"]),
    (&["swift"], &["*.swift"]),
    (&["swig"], &["*.def", "*.i"]),
    (&["systemd"], &[
        "*.automount", "*.conf", "*.device", "*.link", "*.mount", "*.path",
        "*.scope", "*.service", "*.slice", "*.socket", "*.swap", "*.target",
        "*.timer",
    ]),
    (&["taskpaper"], &["*.taskpaper"]),
    (&["tcl"], &["*.tcl"]),
    (&["tex"], &["*.tex", "*.ltx", "*.cls", "*.sty", "*.bib", "*.dtx", "*.ins"]),
    (&["texinfo"], &["*.texi"]),
    (&["textile"], &["*.textile"]),
    (&["tf"], &[
        "*.tf", "*.tf.json", "*.tfvars", "*.tfvars.json",
        "*.terraformrc", "terraform.rc", "*.tfrc", "*.terraform.lock.hcl",
    ]),
    (&["thrift"], &["*.thrift"]),
    (&["toml"], &["*.toml", "Cargo.lock"]),
    (&["ts", "typescript"], &["*.ts", "*.tsx", "*.cts", "*.mts"]),
    (&["twig"], &["*.twig"]),
    (&["txt"], &["*.txt"]),
    (&["typoscript"], &["*.typoscript", "*.ts"]),
    (&["typst"], &["*.typ"]),
    (&["usd"], &["*.usd", "*.usda", "*.usdc"]),
    (&["v"], &["*.v", "*.vsh"]),
    (&["vala"], &["*.vala"]),
    (&["vb"], &["*.vb"]),
    (&["vcl"], &["*.vcl"]),
    (&["verilog"], &["*.v", "*.vh", "*.sv", "*.svh"]),
    (&["vhdl"], &["*.vhd", "*.vhdl"]),
    (&["vim"], &[
        "*.vim", ".vimrc", ".gvimrc", "vimrc", "gvimrc", "_vimrc", "_gvimrc",
    ]),
    (&["vimscript"], &[
        "*.vim", ".vimrc", ".gvimrc", "vimrc", "gvimrc", "_vimrc", "_gvimrc",
    ]),
    (&["vue"], &["*.vue"]),
    (&["webidl"], &["*.idl", "*.webidl", "*.widl"]),
    (&["wgsl"], &["*.wgsl"]),
    (&["wiki"], &["*.mediawiki", "*.wiki"]),
    (&["xml"], &[
        "*.xml", "*.xml.dist", "*.dtd", "*.xsl", "*.xslt", "*.xsd", "*.xjb",
        "*.rng", "*.sch", "*.xhtml",
    ]),
    (&["xz"], &["*.xz", "*.txz"]),
    (&["yacc"], &["*.y"]),
    (&["yaml"], &["*.yaml", "*.yml"]),
    (&["yang"], &["*.yang"]),
    (&["z"], &["*.Z"]),
    (&["zig"], &["*.zig"]),
    (&["zsh"], &[
        ".zshenv", "zshenv",
        ".zlogin", "zlogin",
        ".zlogout", "zlogout",
        ".zprofile", "zprofile",
        ".zshrc", "zshrc",
        "*.zsh",
    ]),
    (&["zstd"], &["*.zst", "*.zstd"]),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> TypeDefs {
        TypeDefs::builtin()
    }

    #[test]
    fn builtin_table_covers_ripgrep_type_count() {
        // ripgrep ships 219 type groups; aliases push the name count higher.
        assert!(
            DEFAULT_TYPES.len() >= 219,
            "expected at least 219 type groups, got {}",
            DEFAULT_TYPES.len()
        );
        assert!(builtin_types().len() > DEFAULT_TYPES.len());
    }

    #[test]
    fn aliases_resolve_to_the_same_globs() {
        let t = builtin_types();
        assert_eq!(t.get("bat"), t.get("batch"));
        assert_eq!(t.get("py"), t.get("python"));
    }

    #[test]
    fn character_class_globs_match() {
        // `c` is defined as `*.[chH]`, which the old prefix matcher could not
        // handle at all.
        let f = defs().build_filter(&["c".to_string()], &[]).unwrap();
        assert!(f.matches("src/main.c"));
        assert!(f.matches("src/main.h"));
        assert!(f.matches("src/main.H"));
        assert!(!f.matches("src/main.rs"));
    }

    #[test]
    fn case_varied_makefile_globs_match() {
        let f = defs().build_filter(&["make".to_string()], &[]).unwrap();
        assert!(f.matches("Makefile"));
        assert!(f.matches("makefile"));
        assert!(f.matches("GNUmakefile"));
        assert!(f.matches("project/Makefile.am"));
    }

    #[test]
    fn negation_takes_precedence_over_selection() {
        let f = defs()
            .build_filter(&["cpp".to_string()], &["c".to_string()])
            .unwrap();
        assert!(f.matches("a.cpp"));
        // `*.h` is in both cpp and c, and -T wins.
        assert!(!f.matches("a.h"));
    }

    #[test]
    fn negation_only_includes_everything_else() {
        let f = defs().build_filter(&[], &["rust".to_string()]).unwrap();
        assert!(!f.matches("main.rs"));
        assert!(f.matches("main.py"));
        assert!(f.matches("README"));
    }

    #[test]
    fn multiple_selected_types_union() {
        let f = defs()
            .build_filter(&["rust".to_string(), "python".to_string()], &[])
            .unwrap();
        assert!(f.matches("a.rs"));
        assert!(f.matches("a.py"));
        assert!(!f.matches("a.go"));
    }

    #[test]
    fn empty_filter_matches_everything() {
        let f = defs().build_filter(&[], &[]).unwrap();
        assert!(f.is_empty());
        assert!(f.matches("anything.xyz"));
    }

    #[test]
    fn unknown_type_is_an_error() {
        let err = defs()
            .build_filter(&["definitely-not-a-type".to_string()], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unrecognized file type"), "got {err}");
    }

    #[test]
    fn type_add_defines_a_new_type() {
        let mut d = defs();
        d.add("foo:*.foo").unwrap();
        let f = d.build_filter(&["foo".to_string()], &[]).unwrap();
        assert!(f.matches("a.foo"));
        assert!(!f.matches("a.bar"));
    }

    #[test]
    fn type_add_appends_to_an_existing_type() {
        let mut d = defs();
        d.add("rust:*.rs.in").unwrap();
        let f = d.build_filter(&["rust".to_string()], &[]).unwrap();
        assert!(f.matches("a.rs"));
        assert!(f.matches("a.rs.in"));
    }

    #[test]
    fn type_add_include_copies_other_types() {
        let mut d = defs();
        d.add("web:include:html,css,js").unwrap();
        let f = d.build_filter(&["web".to_string()], &[]).unwrap();
        assert!(f.matches("a.html"));
        assert!(f.matches("a.css"));
        assert!(f.matches("a.js"));
        assert!(!f.matches("a.rs"));
    }

    #[test]
    fn type_add_include_rejects_unknown_types() {
        let mut d = defs();
        let err = d.add("web:include:nope").unwrap_err().to_string();
        assert!(err.contains("unrecognized file type"), "got {err}");
    }

    #[test]
    fn type_add_rejects_malformed_specs() {
        let mut d = defs();
        assert!(d.add("noseparator").is_err());
        assert!(d.add(":*.foo").is_err());
        assert!(d.add("foo:").is_err());
    }

    #[test]
    fn type_clear_removes_a_definition() {
        let mut d = defs();
        assert!(d.contains("rust"));
        d.clear("rust");
        assert!(!d.contains("rust"));
        assert!(d.build_filter(&["rust".to_string()], &[]).is_err());
    }

    #[test]
    fn globs_do_not_cross_path_separators() {
        let mut d = defs();
        d.add("weird:foo*bar").unwrap();
        let f = d.build_filter(&["weird".to_string()], &[]).unwrap();
        assert!(f.matches("foo-bar"));
        // Only the file name is considered, so a directory called `foo` cannot
        // drag an unrelated file into the type.
        assert!(!f.matches("foo/zzzbar.rs"));
    }

    #[test]
    fn types_added_since_the_old_hardcoded_list() {
        // Spot-check a few of the groups tgrep previously lacked entirely.
        for (name, sample) in [
            ("ada", "lib.adb"),
            ("awk", "script.awk"),
            ("cabal", "pkg.cabal"),
            ("jupyter", "nb.ipynb"),
            ("license", "LICENSE"),
            ("systemd", "unit.service"),
            ("verilog", "top.v"),
            ("vue", "App.vue"),
            ("zsh", ".zshrc"),
        ] {
            let f = defs().build_filter(&[name.to_string()], &[]).unwrap();
            assert!(f.matches(sample), "type `{name}` should match `{sample}`");
        }
    }
}
