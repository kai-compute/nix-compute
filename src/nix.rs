use anyhow::{ensure, Context};
use nix_bindings_expr::{
    eval_state::{gc_register_my_thread, EvalState, EvalStateBuilder, ThreadRegistrationGuard},
    to_json::value_to_json,
    value::Value as NixValue,
};
use nix_bindings_fetchers::FetchersSettings;
use nix_bindings_flake::{
    EvalStateBuilderExt, FlakeLockFlags, FlakeReference, FlakeReferenceParseFlags, FlakeSettings,
    LockedFlake,
};
use nix_bindings_store::store::Store;
use nix_compute_protocol::{
    model::{safe_name, ComputeJob, Target},
    store::path_info,
};
use serde_json::{json, Value};
use std::{fs, path::Path};

/// A small, synchronous owner of the Nix C API state.
///
/// The GC registration must outlive every Nix value and the eval state, so it
/// is deliberately kept in the same owner as the store and evaluator.
struct NixRuntime {
    _gc: ThreadRegistrationGuard,
    store: Store,
    eval_state: EvalState,
    flake_settings: FlakeSettings,
    fetchers: FetchersSettings,
}

impl NixRuntime {
    fn new(reference: Option<&str>) -> anyhow::Result<Self> {
        nix_bindings_expr::eval_state::init()?;
        nix_bindings_util::settings::set("experimental-features", "nix-command flakes")?;
        let gc = gc_register_my_thread()?;
        let store = Store::open(None, [])?;
        let flakes = FlakeSettings::new()?;
        let fetchers = FetchersSettings::new()?;
        let base = reference
            .and_then(path_reference_base)
            .unwrap_or(std::env::current_dir()?);
        let base = base.to_str().context("current directory is not UTF-8")?;
        let eval_state = EvalStateBuilder::new(store.clone())?
            .base_directory(base)?
            .flakes(&flakes)?
            .build()?;
        Ok(Self {
            _gc: gc,
            store,
            eval_state,
            flake_settings: flakes,
            fetchers,
        })
    }

    fn eval(&mut self, expression: &str) -> anyhow::Result<NixValue> {
        self.eval_state
            .eval_from_string(expression, "<nix-compute>")
    }

    fn flake(&mut self, reference: &str) -> anyhow::Result<NixValue> {
        let mut parse_flags = FlakeReferenceParseFlags::new(&self.flake_settings)?;
        parse_flags.set_preserve_relative_paths(true)?;
        if let Some(base) = path_reference_base(reference) {
            parse_flags.set_base_directory(base.to_str().context("flake path is not UTF-8")?)?;
        }
        let (flake_ref, fragment) = FlakeReference::parse_with_fragment(
            &self.fetchers,
            &self.flake_settings,
            &parse_flags,
            reference,
        )?;
        ensure!(
            fragment.is_empty(),
            "flake selector must not contain an output fragment"
        );
        let mut lock_flags = FlakeLockFlags::new(&self.flake_settings)?;
        lock_flags.set_mode_check()?;
        let locked = LockedFlake::lock(
            &self.fetchers,
            &self.flake_settings,
            &self.eval_state,
            &lock_flags,
            &flake_ref,
        )?;
        locked.outputs(&self.flake_settings, &mut self.eval_state)
    }

    fn archive_local(&mut self, path: &Path) -> anyhow::Result<String> {
        let path = serde_json::to_string(path.to_str().context("flake path is not UTF-8")?)?;
        let value = self.eval(&format!(
            "builtins.fetchTree {{ type = \"path\"; path = {path}; }}"
        ))?;
        let archived = self.json(&value)?;
        Ok(archived
            .as_str()
            .context("Nix fetchTree did not return a store path")?
            .to_string())
    }

    fn json(&mut self, value: &NixValue) -> anyhow::Result<serde_json::Value> {
        value_to_json(&mut self.eval_state, value)
    }

    fn realise(&mut self, value: &NixValue) -> anyhow::Result<String> {
        self.eval_state.force(value)?;
        let output = self
            .eval_state
            .require_attrs_select_opt(value, "outPath")?
            .unwrap_or_else(|| value.clone());
        let realised = self.eval_state.realise_string(&output, false)?;
        let path = realised
            .paths
            .first()
            .context("Nix derivation produced no output paths")?;
        self.store.real_path(path)
    }
}

fn path_reference_base(reference: &str) -> Option<std::path::PathBuf> {
    let path = reference.strip_prefix("path:")?.split('?').next()?;
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    path.exists().then(|| path.canonicalize().unwrap_or(path))
}

#[derive(Debug, Clone)]
pub struct Selector {
    pub flake: String,
    pub job: String,
}

impl Selector {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        let (flake, job) = value
            .rsplit_once('#')
            .context("selector must be <flake>#<job>")?;
        ensure!(
            !flake.is_empty() && !flake.starts_with('-') && safe_name(job),
            "invalid flake or job selector"
        );
        Ok(Self {
            flake: flake.into(),
            job: job.into(),
        })
    }

    // Evaluate and build the same immutable source snapshot throughout a run.
    pub fn snapshot(&self) -> anyhow::Result<(Self, Value)> {
        let mut nix = NixRuntime::new(Some(&self.flake))?;
        let _outputs = nix.flake(&self.flake)?;
        let root = if let Some(path) = path_reference_base(&self.flake) {
            nix.archive_local(&path)?
        } else {
            let source_value = nix.eval(&format!(
                "(builtins.getFlake {}).sourceInfo",
                serde_json::to_string(&self.flake)?
            ))?;
            let source = nix.json(&source_value)?;
            source["outPath"]
                .as_str()
                .context("Nix flake sourceInfo has no outPath")?
                .to_string()
        };
        let lock: Value = serde_json::from_slice(
            &fs::read(Path::new(&root).join("flake.lock"))
                .context("submitted Flake must contain flake.lock")?,
        )?;
        let root_node = lock["nodes"]["root"].as_str().unwrap_or("root");
        let locked = lock["nodes"][root_node]["locked"].clone();
        let dir = locked["dir"].as_str().unwrap_or("");
        if !dir.is_empty() {
            crate::model::relative_path(dir)?;
        }
        if let Some(revision) = locked["rev"].as_str() {
            ensure!(
                revision.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid source revision"
            );
        }
        let reference = format!(
            "path:{root}{}",
            if dir.is_empty() {
                String::new()
            } else {
                format!("?dir={dir}")
            }
        );
        let frozen = json!({
            "url": self.flake,
            "locked": locked,
            "revision": locked["rev"],
            "lastModified": locked["lastModified"],
            "path": root,
        });
        let source = path_info(std::slice::from_ref(&root))?;
        Ok((
            Self {
                flake: reference,
                job: self.job.clone(),
            },
            json!({"metadata": frozen, "lock": lock, "source": source, "archive": {"path": root}}),
        ))
    }

    pub fn job(&self) -> anyhow::Result<ComputeJob> {
        let projection = r#"j: if (j.schema_version or 1) != 3 then throw "unsupported schema; migrate inputs to source and rebuild with schema v3" else builtins.removeAttrs j [ "targets" ] // { targets = builtins.mapAttrs (_: t: { inherit (t) name system executor accelerator resources execution; }) j.targets; }"#;
        let apply = format!("jobs: if jobs ? \"{0}\" then ({projection}) jobs.\"{0}\" else if builtins.any (name: builtins.match \".*-(linux|darwin)\" name != null) (builtins.attrNames jobs) then throw \"schema v1 is unsupported; migrate to compute.jobs.<job>.targets and perTarget\" else throw \"unknown compute job {0}\"", self.job);
        let mut nix = NixRuntime::new(Some(&self.flake))?;
        let outputs = nix.flake(&self.flake)?;
        let jobs = nix
            .eval_state
            .require_attrs_select(&outputs, "computeJobs")?;
        let apply = nix.eval(&format!("({apply})"))?;
        let evaluated = nix.eval_state.call(apply, jobs)?;
        let value = nix.json(&evaluated)?;
        let job: ComputeJob = serde_json::from_value(value)?;
        job.validate()?;
        Ok(job)
    }

    pub fn target(&self, name: &str) -> anyhow::Result<Target> {
        ensure!(safe_name(name), "invalid target name");
        let mut nix = NixRuntime::new(Some(&self.flake))?;
        let outputs = nix.flake(&self.flake)?;
        let expression = format!("o: o.computeJobs.\"{}\".targets.\"{name}\"", self.job);
        let selector = nix.eval(&expression)?;
        let evaluated = nix.eval_state.call(selector, outputs)?;
        let target: Target = serde_json::from_value(nix.json(&evaluated)?)?;
        target.validate()?;
        Ok(target)
    }

    pub fn build(&self, output: &str) -> anyhow::Result<String> {
        let mut nix = NixRuntime::new(Some(&self.flake))?;
        let outputs = nix.flake(&self.flake)?;
        let selector = nix.eval(&format!("o: o.{output}"))?;
        let value = nix.eval_state.call(selector, outputs)?;
        let path = nix.realise(&value)?;
        ensure!(
            path.starts_with("/nix/store/"),
            "expected one Nix output path"
        );
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors_cannot_inject_nix_attributes() {
        assert!(Selector::parse(".#train").is_ok());
        for selector in ["#train", "flake", ".#x.y", ".#\"evil\"", "--expr#train"] {
            assert!(Selector::parse(selector).is_err());
        }
    }
}
