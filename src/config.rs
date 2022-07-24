use std::path::{Path, PathBuf};
use std::str::FromStr;

use cargo_toml::Error as CargoTomlError;
use cargo_toml::Manifest;
use rpm::{Compressor, Dependency, RPMBuilder};
use toml::value::Table;
use toml::Value;

use crate::auto_req::{find_requires, AutoReqMode};
use crate::build_target::BuildTarget;
use crate::error::{ConfigError, Error};
use crate::file_info::FileInfo;

#[derive(Debug)]
struct MetadataConfig<'a> {
    metadata: &'a Table,
    branch_path: Option<String>,
}

impl<'a> MetadataConfig<'a> {
    pub fn new_from_manifest(manifest: &'a Manifest) -> Result<Self, Error> {
        let pkg = manifest
            .package
            .as_ref()
            .ok_or(ConfigError::Missing("package".to_string()))?;
        let metadata = pkg
            .metadata
            .as_ref()
            .ok_or(ConfigError::Missing("package.metadata".to_string()))?
            .as_table()
            .ok_or(ConfigError::WrongType(
                "package.metadata".to_string(),
                "table",
            ))?;
        let metadata = metadata
            .iter()
            .find(|(name, _)| name.as_str() == "generate-rpm")
            .ok_or(ConfigError::Missing(
                "package.metadata.generate-rpm".to_string(),
            ))?
            .1
            .as_table()
            .ok_or(ConfigError::WrongType(
                "package.metadata.generate-rpm".to_string(),
                "table",
            ))?;

        Ok(Self {
            metadata,
            branch_path: Some("package.metadata.generate-rpm".to_string()),
        })
    }

    fn create_config_error(&self, name: &str, type_name: &'static str) -> ConfigError {
        let toml_path = self
            .branch_path
            .as_ref()
            .map(|v| [v, name].join("."))
            .unwrap_or(name.to_string());
        ConfigError::WrongType(toml_path, type_name)
    }

    fn get_str(&self, name: &str) -> Result<Option<&'a str>, ConfigError> {
        self.metadata
            .get(name)
            .map(|val| match val {
                Value::String(v) => Ok(Some(v.as_str())),
                _ => Err(self.create_config_error(name, "string")),
            })
            .unwrap_or(Ok(None))
    }

    fn get_i64(&self, name: &str) -> Result<Option<i64>, ConfigError> {
        self.metadata
            .get(name)
            .map(|val| match val {
                Value::Integer(v) => Ok(Some(*v)),
                _ => Err(self.create_config_error(name, "integer")),
            })
            .unwrap_or(Ok(None))
    }

    fn get_string_or_i64(&self, name: &str) -> Result<Option<String>, ConfigError> {
        self.metadata
            .get(name)
            .map(|val| match val {
                Value::String(v) => Ok(Some(v.clone())),
                Value::Integer(v) => Ok(Some(v.to_string())),
                _ => Err(self.create_config_error(name, "string or integer")),
            })
            .unwrap_or(Ok(None))
    }

    fn get_table(&self, name: &str) -> Result<Option<&'a Table>, ConfigError> {
        self.metadata
            .get(name)
            .map(|val| match val {
                Value::Table(v) => Ok(Some(v)),
                _ => Err(self.create_config_error(name, "string or integer")),
            })
            .unwrap_or(Ok(None))
    }

    fn get_array(&self, name: &str) -> Result<Option<&'a [Value]>, ConfigError> {
        self.metadata
            .get(name)
            .map(|val| match val {
                Value::Array(v) => Ok(Some(v.as_slice())),
                _ => Err(self.create_config_error(name, "array")),
            })
            .unwrap_or(Ok(None))
    }
}

#[derive(Debug)]
pub struct RpmBuilderConfig<'a, 'b> {
    build_target: &'a BuildTarget,
    auto_req_mode: AutoReqMode,
    payload_compress: &'b str,
}

impl<'a, 'b> RpmBuilderConfig<'a, 'b> {
    pub fn new(
        build_target: &'a BuildTarget,
        auto_req_mode: AutoReqMode,
        payload_compress: &'b str,
    ) -> RpmBuilderConfig<'a, 'b> {
        RpmBuilderConfig {
            build_target,
            auto_req_mode,
            payload_compress,
        }
    }
}

#[derive(Debug)]
pub struct Config {
    manifest: Manifest,
    path: PathBuf,
}

impl Config {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        Manifest::from_path(&path)
            .map(|manifest| Config {
                manifest,
                path: path.clone(),
            })
            .map_err(|err| match err {
                CargoTomlError::Io(e) => Error::FileIo(path, e),
                _ => Error::CargoToml(err),
            })
    }

    fn table_to_dependencies(table: &Table) -> Result<Vec<Dependency>, ConfigError> {
        let mut dependencies = Vec::with_capacity(table.len());
        for (key, value) in table {
            let ver = value
                .as_str()
                .ok_or(ConfigError::WrongDependencyVersion(key.clone()))?
                .trim();
            let ver_vec = ver.trim().split_whitespace().collect::<Vec<_>>();
            let dependency = match ver_vec.as_slice() {
                [] | ["*"] => Ok(Dependency::any(key)),
                ["<", ver] => Ok(Dependency::less(key.as_str(), ver.trim())),
                ["<=", ver] => Ok(Dependency::less_eq(key.as_str(), ver.trim())),
                ["=", ver] => Ok(Dependency::eq(key.as_str(), ver.trim())),
                [">", ver] => Ok(Dependency::greater(key.as_str(), ver.trim())),
                [">=", ver] => Ok(Dependency::greater_eq(key.as_str(), ver.trim())),
                _ => Err(ConfigError::WrongDependencyVersion(key.clone())),
            }?;
            dependencies.push(dependency);
        }
        Ok(dependencies)
    }

    pub fn create_rpm_builder(
        &self,
        rpm_builder_config: RpmBuilderConfig,
    ) -> Result<RPMBuilder, Error> {
        let metadata = MetadataConfig::new_from_manifest(&self.manifest)?;

        let pkg = self
            .manifest
            .package
            .as_ref()
            .ok_or(ConfigError::Missing("package".to_string()))?;
        let name = metadata
            .get_str("name")?
            .unwrap_or_else(|| pkg.name.as_str());
        let version = metadata
            .get_str("version")?
            .unwrap_or_else(|| pkg.version.as_str());
        let license = metadata
            .get_str("license")?
            .or_else(|| pkg.license.as_ref().map(|v| v.as_ref()))
            .ok_or(ConfigError::Missing("package.license".to_string()))?;
        let arch = rpm_builder_config.build_target.binary_arch();
        let desc = metadata
            .get_str("summary")?
            .or_else(|| pkg.description.as_ref().map(|v| v.as_ref()))
            .ok_or(ConfigError::Missing("package.description".to_string()))?;
        let assets = metadata
            .get_array("assets")?
            .ok_or(ConfigError::Missing("package.assets".to_string()))?;
        let files = FileInfo::new(assets)?;
        let parent = self.path.parent().unwrap();

        let mut builder = RPMBuilder::new(name, version, license, arch.as_str(), desc)
            .compression(Compressor::from_str(rpm_builder_config.payload_compress)?);
        for file in &files {
            let file_source =
                file.generate_rpm_file_path(rpm_builder_config.build_target, parent)?;
            let options = file.generate_rpm_file_options();
            builder = builder.with_file(file_source, options)?;
        }

        if let Some(release) = metadata.get_string_or_i64("release")? {
            builder = builder.release(release);
        }
        if let Some(epoch) = metadata.get_i64("epoch")? {
            builder = builder.epoch(epoch as i32);
        }

        if let Some(pre_install_script) = metadata.get_str("pre_install_script")? {
            builder = builder.pre_install_script(pre_install_script);
        }
        if let Some(pre_uninstall_script) = metadata.get_str("pre_uninstall_script")? {
            builder = builder.pre_uninstall_script(pre_uninstall_script);
        }
        if let Some(post_install_script) = metadata.get_str("post_install_script")? {
            builder = builder.post_install_script(post_install_script);
        }
        if let Some(post_uninstall_script) = metadata.get_str("post_uninstall_script")? {
            builder = builder.post_uninstall_script(post_uninstall_script);
        }

        if let Some(requires) = metadata.get_table("requires")? {
            for dependency in Self::table_to_dependencies(requires)? {
                builder = builder.requires(dependency);
            }
        }
        let auto_req = if rpm_builder_config.auto_req_mode == AutoReqMode::Auto
            && matches!(metadata.get_str("auto-req")?, Some("no") | Some("disabled"))
        {
            AutoReqMode::Disabled
        } else {
            rpm_builder_config.auto_req_mode
        };
        for requires in find_requires(files.iter().map(|v| Path::new(&v.source)), auto_req)? {
            builder = builder.requires(Dependency::any(requires));
        }
        if let Some(obsoletes) = metadata.get_table("obsoletes")? {
            for dependency in Self::table_to_dependencies(obsoletes)? {
                builder = builder.obsoletes(dependency);
            }
        }
        if let Some(conflicts) = metadata.get_table("conflicts")? {
            for dependency in Self::table_to_dependencies(conflicts)? {
                builder = builder.conflicts(dependency);
            }
        }
        if let Some(provides) = metadata.get_table("provides")? {
            for dependency in Self::table_to_dependencies(provides)? {
                builder = builder.provides(dependency);
            }
        }

        Ok(builder)
    }
}

#[cfg(test)]
mod test {
    use cargo_toml::Value;

    use super::*;

    #[test]
    fn test_metadata_config() {
        let metadata = r#"
            str = "str"
            int = 256
            table = { int = 128 }
            array = [ 1, 2 ]
        "#
        .parse::<Value>()
        .unwrap();
        let metadata_config = MetadataConfig {
            metadata: metadata.as_table().unwrap(),
            branch_path: None,
        };

        assert_eq!(metadata_config.get_str("str").unwrap(), Some("str"));
        assert_eq!(metadata_config.get_i64("int").unwrap(), Some(256));
        assert_eq!(
            metadata_config.get_string_or_i64("str").unwrap(),
            Some("str".to_string())
        );
        assert_eq!(
            metadata_config.get_string_or_i64("int").unwrap(),
            Some("256".to_string())
        );
        assert_eq!(
            metadata_config.get_table("table").unwrap(),
            "int = 128".parse::<Value>().unwrap().as_table()
        );
        assert_eq!(
            metadata_config.get_array("array").unwrap().unwrap(),
            [Value::Integer(1), Value::Integer(2)]
        );

        assert_eq!(metadata_config.get_str("not-exist").unwrap(), None);
        assert!(matches!(
            metadata_config.get_str("int"),
            Err(ConfigError::WrongType(v, "string")) if v == "int".to_string()
        ));
        assert!(matches!(
            metadata_config.get_string_or_i64("array"),
            Err(ConfigError::WrongType(v, "string or integer")) if v == "array".to_string()
        ));

        let metadata_config = MetadataConfig {
            metadata: metadata.as_table().unwrap(),
            branch_path: Some("branch".to_string()),
        };
        assert!(matches!(
            metadata_config.get_str("int"),
            Err(ConfigError::WrongType(v, "string")) if v == "branch.int".to_string()
        ));
        assert!(matches!(
            metadata_config.get_string_or_i64("array"),
            Err(ConfigError::WrongType(v, "string or integer")) if v == "branch.array".to_string()
        ));
    }

    #[test]
    fn test_config_new() {
        let config = Config::new("Cargo.toml").unwrap();
        let pkg = config.manifest.package.unwrap();
        assert_eq!(pkg.name, "cargo-generate-rpm");

        assert!(matches!(Config::new("not_exist_path/Cargo.toml"),
            Err(Error::FileIo(path, error)) if path == PathBuf::from("not_exist_path/Cargo.toml") && error.kind() == std::io::ErrorKind::NotFound));
        assert!(matches!(
            Config::new("src/error.rs"),
            Err(Error::CargoToml(_))
        ));
    }

    #[test]
    fn test_new() {
        let config = Config::new("Cargo.toml").unwrap();
        assert_eq!(config.manifest.package.unwrap().name, "cargo-generate-rpm");
        assert_eq!(config.path, PathBuf::from("Cargo.toml"));
    }

    #[test]
    fn test_table_to_dependencies() {
        fn dependency_to_u8_slice(dep: &Dependency) -> &[u8] {
            unsafe { std::mem::transmute_copy(dep) }
        }

        let mut table = Table::new();
        [
            ("any1", ""),
            ("any2", "*"),
            ("less", "< 1.0"),
            ("lesseq", "<= 1.0"),
            ("eq", "= 1.0"),
            ("greater", "> 1.0"),
            ("greatereq", "<= 1.0"),
        ]
        .iter()
        .for_each(|(k, v)| {
            table.insert(k.to_string(), Value::String(v.to_string()));
        });

        assert_eq!(
            Config::table_to_dependencies(&table)
                .unwrap()
                .iter()
                .map(&dependency_to_u8_slice)
                .collect::<Vec<_>>(),
            vec![
                dependency_to_u8_slice(&Dependency::any("any1")),
                dependency_to_u8_slice(&Dependency::any("any2")),
                dependency_to_u8_slice(&Dependency::eq("eq", "1.0")),
                dependency_to_u8_slice(&Dependency::greater("greater", "1.0")),
                dependency_to_u8_slice(&Dependency::greater_eq("greatereq", "1.0")),
                dependency_to_u8_slice(&Dependency::less("less", "1.0")),
                dependency_to_u8_slice(&Dependency::less_eq("lesseq", "1.0")),
            ]
        );

        // table.clear();
        table.insert("error".to_string(), Value::Integer(1));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));

        table.clear();
        table.insert("error".to_string(), Value::String("1".to_string()));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));

        table.clear();
        table.insert("error".to_string(), Value::String("!= 1".to_string()));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));

        table.clear();
        table.insert("error".to_string(), Value::String("> 1 1".to_string()));
        assert!(matches!(
            Config::table_to_dependencies(&table),
            Err(ConfigError::WrongDependencyVersion(_))
        ));
    }

    #[test]
    fn test_config_create_rpm_builder() {
        let config = Config::new("Cargo.toml").unwrap();
        let builder = config.create_rpm_builder(RpmBuilderConfig::new(
            &BuildTarget::default(),
            AutoReqMode::Disabled,
            "zstd",
        ));

        assert!(if Path::new("target/release/cargo-generate-rpm").exists() {
            matches!(builder, Ok(_))
        } else {
            matches!(builder, Err(Error::Config(ConfigError::AssetFileNotFound(path))) if path == "target/release/cargo-generate-rpm")
        });
    }
}
