use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Frontend module loading mode matching Decky's `PluginLoadType` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PluginLoadType {
    LegacyEvalIife = 0,
    EsModuleV1 = 1,
}

impl Serialize for PluginLoadType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for PluginLoadType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u8::deserialize(deserializer)? {
            0 => Ok(Self::LegacyEvalIife),
            1 => Ok(Self::EsModuleV1),
            other => Err(serde::de::Error::custom(format!(
                "unknown PluginLoadType code: {other}"
            ))),
        }
    }
}

/// Parsed `plugin.json` manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub author: String,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub api_version: u32,
}

impl PluginManifest {
    /// Returns `true` if the plugin requests elevated `root` privileges.
    #[must_use]
    pub fn requires_root(&self) -> bool {
        self.flags.iter().any(|flag| flag == "root")
    }

    /// Returns `true` if the plugin opts into live hot-reload in debug mode.
    #[must_use]
    pub fn is_debug(&self) -> bool {
        self.flags.iter().any(|flag| flag == "debug")
    }
}

/// Optional `package.json` metadata used to detect plugin version, ES module mode, and binaries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PackageManifest {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(rename = "type", default)]
    pub package_type: Option<String>,
    #[serde(default)]
    pub remote_binary: Vec<RemoteBinarySpec>,
}

impl PackageManifest {
    /// Resolves the frontend bundle loading strategy for this plugin.
    #[must_use]
    pub fn load_type(&self) -> PluginLoadType {
        if self.package_type.as_deref() == Some("module") {
            PluginLoadType::EsModuleV1
        } else {
            PluginLoadType::LegacyEvalIife
        }
    }
}

/// Specification for an external binary downloaded during plugin installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBinarySpec {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub sha256hash: Option<String>,
}
