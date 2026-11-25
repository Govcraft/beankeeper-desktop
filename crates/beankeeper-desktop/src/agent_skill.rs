//! Publish the same skill that agent registration installs locally (SEP-2640).

use std::{str::FromStr, sync::LazyLock};

use rmcp::{
    ErrorData,
    model::{
        CacheScope, ErrorCode, GetSkillRequestParams, GetSkillResult, ListResourceTemplatesResult,
        ListResourcesResult, ListSkillsResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResult, Resource, ResourceContents, SKILLS_EXTENSION_ID, Skill,
        SkillFrontmatter, SkillResource, SkillResources,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const SKILL: &str = include_str!("../../../SKILL.md");
const URI: &str = "skill://beankeeper-desktop/SKILL.md";
const MIME_TYPE: &str = "text/markdown";
const _: () = assert!(SKILL.len() <= 16 * 1024 * 1024);
static EMBEDDED: LazyLock<Result<EmbeddedSkill, ErrorData>> =
    LazyLock::new(|| EmbeddedSkill::try_from(SKILL));

struct SkillExcerpt(String);

impl TryFrom<(&str, &str)> for SkillExcerpt {
    type Error = ErrorData;

    fn try_from((markdown, marker): (&str, &str)) -> Result<Self, Self::Error> {
        let paragraph = markdown
            .lines()
            .skip_while(|line| *line != marker)
            .skip(1)
            .map(str::trim)
            .skip_while(|line| line.is_empty())
            .take_while(|line| {
                !line.is_empty() && !line.starts_with('#') && !line.starts_with("- ")
            })
            .collect::<Vec<_>>()
            .join(" ");
        if paragraph.is_empty() {
            return Err(ErrorData::internal_error(
                format!("Missing embedded Beankeeper skill excerpt after {marker:?}."),
                None,
            ));
        }
        Ok(Self(paragraph))
    }
}

struct SkillDescription(String);

impl TryFrom<String> for SkillDescription {
    type Error = ErrorData;

    fn try_from(description: String) -> Result<Self, Self::Error> {
        if (1..=1024).contains(&description.chars().count()) {
            Ok(Self(description))
        } else {
            Err(ErrorData::internal_error(
                "The embedded Beankeeper skill description must have 1 to 1024 characters.",
                None,
            ))
        }
    }
}

impl From<SkillDescription> for String {
    fn from(description: SkillDescription) -> Self {
        description.0
    }
}

/// The private fields tie the manifest, advertised digest and handshake excerpts to the same content.
pub(crate) struct EmbeddedSkill {
    entry: Skill,
    markdown: &'static str,
    voice: SkillExcerpt,
    claude_setup: SkillExcerpt,
    demo: SkillExcerpt,
}

impl TryFrom<&'static str> for EmbeddedSkill {
    type Error = ErrorData;

    fn try_from(markdown: &'static str) -> Result<Self, Self::Error> {
        let invalid = || ErrorData::internal_error("Invalid embedded Beankeeper skill.", None);
        let (opening, rest) = markdown.split_once('\n').ok_or_else(invalid)?;
        let yaml_len = rest
            .split_inclusive('\n')
            .scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len();
                Some((start, line))
            })
            .find_map(|(start, line)| {
                (line.trim_end_matches(['\r', '\n']) == "---").then_some(start)
            });
        let yaml_len = yaml_len
            .filter(|_| opening.trim_end_matches('\r') == "---")
            .ok_or_else(invalid)?;
        let mut frontmatter: SkillFrontmatter =
            yaml_serde::from_str(&rest[..yaml_len]).map_err(|_| invalid())?;
        frontmatter.description = SkillDescription::try_from(frontmatter.description)?.into();
        let files =
            SkillResources::Files(vec![SkillResource::from_bytes(URI, markdown.as_bytes())]);
        let entry = Skill::new(URI, frontmatter, files);
        entry.validate().map_err(|_| invalid())?;
        Ok(Self {
            entry,
            markdown,
            voice: SkillExcerpt::try_from((markdown, "## How to talk to the owner"))?,
            claude_setup: SkillExcerpt::try_from((markdown, "- **Claude Desktop chat.**"))?,
            demo: SkillExcerpt::try_from((markdown, "### Tailor a demo in chat"))?,
        })
    }
}

impl EmbeddedSkill {
    pub(crate) fn get() -> Result<&'static Self, ErrorData> {
        EMBEDDED.as_ref().map_err(Clone::clone)
    }

    pub(crate) fn voice(&self) -> &str {
        &self.voice.0
    }

    pub(crate) fn claude_setup(&self) -> &str {
        &self.claude_setup.0
    }

    pub(crate) fn demo(&self) -> &str {
        &self.demo.0
    }

    fn require_uri(&self, uri: &str) -> Result<(), ErrorData> {
        if uri == self.entry.uri {
            Ok(())
        } else {
            Err(ErrorData::invalid_params(
                "Only the Beankeeper Desktop skill is served here.",
                None,
            ))
        }
    }

    fn resource(&self) -> Resource {
        Resource::new(&self.entry.uri, self.entry.name())
            .with_description(&self.entry.frontmatter.description)
            .with_mime_type(MIME_TYPE)
            .with_size(self.markdown.len() as u64)
    }
}

/// Requests answered by the embedded skill catalog.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillMethod {
    SkillsList,
    SkillsGet,
    ResourcesList,
    ResourcesRead,
    ResourceTemplatesList,
}

impl FromStr for SkillMethod {
    type Err = ErrorCode;

    fn from_str(method: &str) -> Result<Self, Self::Err> {
        match method {
            "skills/list" => Ok(Self::SkillsList),
            "skills/get" => Ok(Self::SkillsGet),
            "resources/list" => Ok(Self::ResourcesList),
            "resources/read" => Ok(Self::ResourcesRead),
            "resources/templates/list" => Ok(Self::ResourceTemplatesList),
            _ => Err(ErrorCode::METHOD_NOT_FOUND),
        }
    }
}

pub(crate) fn advertise(capabilities: &mut Value) {
    capabilities["extensions"][SKILLS_EXTENSION_ID] = json!({});
    if capabilities.get("resources").is_none() {
        capabilities["resources"] = json!({});
    }
}

pub(crate) fn result(method: SkillMethod, params: &Value) -> Result<Value, ErrorData> {
    let skill = EmbeddedSkill::get()?;
    match method {
        SkillMethod::SkillsGet => {
            let request = parse::<GetSkillRequestParams>(params)?;
            skill.require_uri(&request.uri)?;
            encode(
                GetSkillResult::new(skill.entry.clone())
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Public),
            )
        }
        SkillMethod::ResourcesRead => {
            let request = parse::<ReadResourceRequestParams>(params)?;
            skill.require_uri(&request.uri)?;
            let contents =
                ResourceContents::text(skill.markdown, &skill.entry.uri).with_mime_type(MIME_TYPE);
            encode(
                ReadResourceResult::new(vec![contents])
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Public),
            )
        }
        SkillMethod::SkillsList => {
            require_first_page(params)?;
            encode(
                ListSkillsResult::with_all_items(vec![skill.entry.clone()])
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Public),
            )
        }
        SkillMethod::ResourcesList => {
            require_first_page(params)?;
            encode(
                ListResourcesResult::with_all_items(vec![skill.resource()])
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Public),
            )
        }
        SkillMethod::ResourceTemplatesList => {
            require_first_page(params)?;
            encode(
                ListResourceTemplatesResult::with_all_items(vec![])
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Public),
            )
        }
    }
}

fn require_first_page(params: &Value) -> Result<(), ErrorData> {
    let request = parse::<Option<PaginatedRequestParams>>(params)?;
    if request.is_some_and(|page| page.cursor.is_some()) {
        return Err(ErrorData::invalid_params(
            "The Beankeeper skill catalog has one page.",
            None,
        ));
    }
    Ok(())
}

fn parse<'de, T: Deserialize<'de>>(params: &'de Value) -> Result<T, ErrorData> {
    T::deserialize(params).map_err(|_| ErrorData::invalid_params("Invalid skill arguments.", None))
}

fn encode(result: impl Serialize) -> Result<Value, ErrorData> {
    serde_json::to_value(result)
        .map_err(|_| ErrorData::internal_error("Cannot serialize the Beankeeper skill.", None))
}

#[cfg(test)]
mod tests {
    use super::{EmbeddedSkill, ErrorData, SkillExcerpt};

    #[test]
    fn embedded_skill_contains_all_handshake_excerpts() -> Result<(), ErrorData> {
        let skill = EmbeddedSkill::get()?;
        for excerpt in [skill.voice(), skill.claude_setup(), skill.demo()] {
            assert_ne!(excerpt.trim(), "");
        }
        Ok(())
    }

    #[test]
    fn excerpt_requires_a_marker_and_a_paragraph() {
        let marker = "## Required guidance";
        assert!(SkillExcerpt::try_from(("## Another heading\n\nText.", marker)).is_err());
        assert!(
            SkillExcerpt::try_from(("## Required guidance\n\n## Next section\n\nText.", marker))
                .is_err()
        );
    }
}
