use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    User,
    Feedback,
    Project,
    Reference,
    SystemState,
    Network,
    Device,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
            Self::SystemState => "system_state",
            Self::Network => "network",
            Self::Device => "device",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            "system_state" => Some(Self::SystemState),
            "network" => Some(Self::Network),
            "device" => Some(Self::Device),
            _ => None,
        }
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub category: Category,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub priority: i32,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateMemory {
    pub category: Category,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default)]
    pub expires_at: Option<String>,
}

fn default_priority() -> i32 {
    5
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemory {
    pub title: Option<String>,
    pub content: Option<String>,
    pub category: Option<Category>,
    pub tags: Option<Vec<String>>,
    pub priority: Option<i32>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub category: Option<Category>,
    #[serde(default = "default_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub min_priority: Option<i32>,
}

fn default_limit() -> Option<usize> {
    Some(20)
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub category: Option<Category>,
    #[serde(default = "default_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub min_priority: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub total: usize,
    pub by_category: Vec<CategoryCount>,
    pub db_size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct CategoryCount {
    pub category: String,
    pub count: usize,
}
