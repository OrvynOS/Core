//! Hosting tiers and the job categories they unlock. The tier lives in the Orvyn registry (Basic 0,
//! Pro 1, Studio 2), paid at the tier's price. It decides what the agent runs on (its runtime plan
//! and its share of Orvyn Studio) and therefore which marketplace jobs it can take.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Basic,
    Pro,
    Studio,
}

pub const TIER_ORDER: [Tier; 3] = [Tier::Basic, Tier::Pro, Tier::Studio];

impl Tier {
    pub fn index(self) -> u8 {
        self as u8
    }
    pub fn from_index(i: u64) -> Tier {
        TIER_ORDER.get(i as usize).copied().unwrap_or(Tier::Basic)
    }
    pub fn parse(s: &str) -> Tier {
        match s {
            "pro" => Tier::Pro,
            "studio" => Tier::Studio,
            _ => Tier::Basic,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Basic => "basic",
            Tier::Pro => "pro",
            Tier::Studio => "studio",
        }
    }
    pub fn at_least(self, need: Tier) -> bool {
        self >= need
    }
    pub fn spec(self) -> &'static TierSpec {
        &TIERS[self as usize]
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioLimits {
    pub renderers: &'static [&'static str],
    pub max_render_seconds: u32,
    pub max_video_seconds: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub max_output_mb: u32,
    pub renders_per_job: u32,
    pub workers: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Runtime {
    pub plan: &'static str,
    pub vcpu: f64,
    #[serde(rename = "memoryGb")]
    pub memory_gb: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TierSpec {
    pub id: Tier,
    pub index: u8,
    pub name: &'static str,
    pub usd: u32,
    pub bps: u32,
    pub runtime: Runtime,
    pub studio: StudioLimits,
    pub summary: &'static str,
}

const BASIC_RENDERERS: &[&str] = &["diagram", "html-still", "design-review"];
const PRO_RENDERERS: &[&str] = &["diagram", "html-still", "design-review", "ffmpeg", "html-video", "reel", "manim"];
const STUDIO_RENDERERS: &[&str] = &["diagram", "html-still", "design-review", "ffmpeg", "html-video", "reel", "manim", "blender", "ai-video", "world-3d", "world-video", "long-video"];

pub static TIERS: [TierSpec; 3] = [
    TierSpec {
        id: Tier::Basic,
        index: 0,
        name: "Basic",
        usd: 20,
        bps: 10_000,
        runtime: Runtime { plan: "starter", vcpu: 0.5, memory_gb: 0.5 },
        studio: StudioLimits { renderers: BASIC_RENDERERS, max_render_seconds: 120, max_video_seconds: 0, max_width: 2560, max_height: 2560, max_output_mb: 50, renders_per_job: 10, workers: "cpu" },
        summary: "Research, writing, analysis, diagrams, graphics and design reviews.",
    },
    TierSpec {
        id: Tier::Pro,
        index: 1,
        name: "Pro",
        usd: 60,
        bps: 30_000,
        runtime: Runtime { plan: "standard", vcpu: 1.0, memory_gb: 2.0 },
        studio: StudioLimits { renderers: PRO_RENDERERS, max_render_seconds: 900, max_video_seconds: 180, max_width: 1920, max_height: 1920, max_output_mb: 500, renders_per_job: 30, workers: "cpu" },
        summary: "Everything in Basic, plus video edits, motion graphics, explainer videos and UI builds.",
    },
    TierSpec {
        id: Tier::Studio,
        index: 2,
        name: "Studio",
        usd: 150,
        bps: 75_000,
        runtime: Runtime { plan: "pro", vcpu: 2.0, memory_gb: 4.0 },
        studio: StudioLimits { renderers: STUDIO_RENDERERS, max_render_seconds: 3600, max_video_seconds: 600, max_width: 3840, max_height: 3840, max_output_mb: 2000, renders_per_job: 60, workers: "gpu" },
        summary: "Everything in Pro, plus 3D (Blender) and AI video on GPU workers.",
    },
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCategory {
    pub id: &'static str,
    pub name: &'static str,
    pub min_tier: Tier,
    pub deliverables: &'static str,
    pub renderers: &'static [&'static str],
    pub guidance: &'static str,
}

macro_rules! cat {
    ($id:expr, $name:expr, $tier:ident, $del:expr, [$($r:expr),*], $g:expr) => {
        JobCategory { id: $id, name: $name, min_tier: Tier::$tier, deliverables: $del, renderers: &[$($r),*], guidance: $g }
    };
}

pub static JOB_CATEGORIES: &[JobCategory] = &[
    cat!("research", "Research and reports", Basic, "A written report (Markdown), with sources.", [], "Every figure from a tool, with where and when it was fetched. Say what you couldn't find."),
    cat!("market-analysis", "Market and on-chain analysis", Basic, "A written analysis with the data behind it, and charts if asked.", ["html-still"], "Measure with tools, then judge. Label opinions as opinions; no promises of returns."),
    cat!("writing", "Writing", Basic, "Text: posts, threads, docs, copy.", [], "Write for the client's reader. Concrete over generic; cut filler and stock phrases."),
    cat!("diagrams", "Diagrams", Basic, "SVG and PNG diagrams, plus the editable Excalidraw file.", ["diagram"], "Show the real mechanism: label every arrow, keep one idea per diagram, check it reads at the size it'll be shown."),
    cat!("graphics", "Graphics and social cards", Basic, "PNG images at the sizes asked for.", ["html-still"], "Lay out in HTML/CSS and render; check text fits and contrast holds at every size delivered."),
    cat!("design-review", "Design reviews", Basic, "A review of a page or screens: screenshots at several widths and a ranked list of issues with fixes.", ["design-review", "html-still"], "Look at the rendered page, not just the code. Rank issues by what a user would hit first; every issue gets a concrete fix."),
    cat!("ui-build", "UI builds", Pro, "Source files (HTML/CSS/JS or React) plus screenshots at phone and desktop widths.", ["html-still", "design-review", "html-video"], "Build, render, review your own screenshots, fix, then deliver. No horizontal scroll at phone width."),
    cat!("video-edit", "Video editing", Pro, "An MP4 (H.264/AAC) cut from the client's footage: trims, joins, captions, audio.", ["ffmpeg"], "Work from the client's files only. Probe inputs first; keep frame rate and audio sync; deliver within the length asked."),
    cat!("motion", "Motion graphics", Pro, "An MP4 animation (and a GIF or WebM if asked).", ["reel", "html-video", "ffmpeg"], "Build scenes as a Reel film (clips, tweens, springs, transitions) or a GSAP/CSS timeline; everything is drawn from the time, so every frame renders exactly."),
    cat!("explainer-video", "Explainer videos", Pro, "An MP4 explainer with narration or captions.", ["reel", "manim", "html-video", "ffmpeg"], "Script first, then scenes. Reel for the film (scenes, narration track, captions), Manim for maths and charts. Captions on by default."),
    cat!("games", "Games", Pro, "A playable game made with Orvyn Engine, saved in the client's account to publish (free, paid, token-gated or with tips).", [], "Agree on the idea in one message if the brief is vague. Make it original and winnable: a goal, a way to lose, a HUD, touch controls. Check it until it passes, then save it."),
    cat!("3d", "3D models and scenes", Studio, "GLB models and rendered stills or turntables.", ["blender", "html-video", "ffmpeg"], "Model in Blender headless; check the GLB loads and is under the size asked; render previews from several angles."),
    cat!("ai-video", "AI video", Studio, "Generated video clips (MP4), edited together if asked.", ["ai-video", "long-video", "ffmpeg"], "Generate several takes, pick and say why, and edit. Never imitate a real person's likeness or voice."),
    cat!("worlds", "Worlds", Studio, "Explorable worlds made with open world models: 3D scenes (Gaussian splats or meshes) from an image, walk-through videos along a path, long videos from a timeline.", ["world-3d", "world-video", "long-video", "ffmpeg"], "Ask for (or make) a strong starting image. Plan the path or timeline, render, look at the result, and re-render the parts that drift. Say which model made each file and include its notice."),
];

pub fn job_category(id: &str) -> Option<&'static JobCategory> {
    JOB_CATEGORIES.iter().find(|c| c.id == id)
}

/// Why an agent on `tier` can't offer `category_id`, or None if it can.
pub fn category_blocked(tier: Tier, category_id: &str) -> Option<String> {
    let Some(c) = job_category(category_id) else { return Some("Unknown job category.".into()) };
    if !tier.at_least(c.min_tier) {
        return Some(format!("{} needs {} hosting (this agent is on {}).", c.name, c.min_tier.spec().name, tier.spec().name));
    }
    None
}

pub fn categories_for(tier: Tier) -> Vec<&'static JobCategory> {
    JOB_CATEGORIES.iter().filter(|c| tier.at_least(c.min_tier)).collect()
}
