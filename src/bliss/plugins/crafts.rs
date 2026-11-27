//! Craft packs: Bliss plugins that give an agent a craft (3D with three.js, motion with GSAP,
//! interface design, Blender, Manim, diagrams, video editing) for marketplace jobs. Each one adds
//! a short working brief to the agent's instructions and a `<craft>_guide` tool with the detailed
//! sections, read when a job needs them. A pack names the job categories it covers; some
//! categories need their pack (tiers.rs `required_craft`). The guidance is Orvyn's own, written
//! for Orvyn Studio's renderers (studio_guide.json).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::meta;
use crate::ai::{Tool, ToolSet};
use crate::bliss::{Ctx, Plugin, PluginMeta};
use crate::tiers::job_category;

pub struct Craft {
    pub id: &'static str,
    pub name: &'static str,
    pub tool: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    /// Job categories the pack is for (tiers.rs JOB_CATEGORIES).
    pub categories: &'static [&'static str],
    /// Always in the agent's instructions.
    pub brief: &'static str,
    /// Read on demand with the guide tool.
    pub sections: &'static [(&'static str, &'static str)],
}

pub static CRAFTS: &[Craft] = &[
    Craft {
        id: "craft-threejs",
        name: "3D for the web (three.js)",
        tool: "threejs_guide",
        summary: "Interactive 3D scenes, product turntables and 3D web sections with three.js, rendered and checked in Orvyn Studio.",
        description: "Teaches the agent to build real-time 3D with three.js: scene setup, physically based materials and studio lighting, models from primitives or GLB files, camera moves, and turntable videos. Renders run in Orvyn Studio (html-still for stills, html-video for frame-exact video) with three.js served locally. Unlocks the Interactive 3D category on the marketplace (Pro hosting).",
        categories: &["3d-web", "3d", "ui-build", "motion", "graphics"],
        brief: "three.js craft: import three and three/addons from https://studio.local/lib/ via an import map. Renderer: antialias, ACESFilmic tone mapping, sRGB output, pixel ratio ≤ 2, sized from the container. Light with a PMREM RoomEnvironment plus one key light; physically based materials (roughness/metalness, clearcoat for gloss). Drive every motion from time so html-video renders it exactly. Before delivering, render stills from at least three angles and look at them: no black frames, nothing clipped, the subject fills the frame.",
        sections: &[
            ("setup", "Page: a full-viewport <canvas> or container, an import map { \"three\": \"https://studio.local/lib/three/build/three.module.js\", \"three/addons/\": \"https://studio.local/lib/three/examples/jsm/\" }. WebGLRenderer({ antialias: true, alpha: false unless the client needs transparency }). renderer.toneMapping = ACESFilmicToneMapping; outputColorSpace = SRGBColorSpace; setPixelRatio(Math.min(devicePixelRatio, 2)). PerspectiveCamera fov 30–45 for products (less distortion), 50–60 for environments. Resize on the container (ResizeObserver), not the window. Set colour textures' colorSpace to SRGBColorSpace; leave data textures (normal, roughness) linear."),
            ("lighting", "Start with image-based light: const pmrem = new PMREMGenerator(renderer); scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture. Then a key DirectionalLight (2–3) from above-front-left, an optional rim light behind the subject in the accent colour, and a low HemisphereLight fill. Shadows only when they ground the subject: one shadow-casting light, a tight shadow camera, PCFSoftShadowMap; or a flat contact shadow (a dark ellipse with low opacity under the object), which costs nothing. Avoid pure black: lift shadows with the fill."),
            ("materials", "MeshStandardMaterial for most things; MeshPhysicalMaterial for gloss (clearcoat 1, clearcoatRoughness 0.1–0.2), glass (transmission 1, thickness, ior 1.45) and fabric (sheen). Roughness carries realism: 0.15 polished, 0.4 satin, 0.8 matte. Metalness is 0 or 1 in reality; in between reads as dirty. Keep a palette of 2–3 materials per scene and reuse material instances. Emissive only for things that actually glow (LEDs, screens)."),
            ("models", "Build from primitives when the brief allows (RoundedBoxGeometry, CylinderGeometry, TorusGeometry, LatheGeometry for turned shapes, ExtrudeGeometry for logos from SVG paths via SVGLoader). Client models: GLTFLoader from the job's files; centre with Box3, scale to a known size, then frame the camera from the bounding sphere. Many copies: InstancedMesh. Keep a product under ~200k triangles; check with renderer.info."),
            ("animation", "In Studio, time is virtual: read performance.now() (or a THREE.Clock) inside requestAnimationFrame and derive every position from t. Never accumulate per-frame deltas for a video (drift); compute from t. Turntable: object.rotation.y = t / seconds * Math.PI * 2 so the loop closes exactly. Camera moves: interpolate along a CatmullRomCurve3 with an ease (smoothstep), always lookAt a target. Ease motion; nothing starts or stops at full speed."),
            ("delivery", "Interactive scenes: deliver index.html plus assets that run from a static host, with OrbitControls (damping on, limits on zoom and polar angle) or pointer parallax, a reduced-motion still, and a fallback message without WebGL. Always add stills (html-still at 1920x1080 and a square 1080) and a short turntable or flythrough (html-video, 6–12 s, 30 fps). Look at every render; fix and re-render before deliver_result. Dispose geometries, materials and textures when tearing a scene down."),
            ("performance", "Budget for a mid phone: under 100 draw calls, textures ≤ 2048, one shadow map. Merge static meshes (BufferGeometryUtils.mergeGeometries), share materials, use InstancedMesh for repeats. Pause rendering when the canvas is off screen (IntersectionObserver) or the tab is hidden. Cap pixel ratio at 2. Prefer baked lighting (a texture) to many real-time lights."),
        ],
    },
    Craft {
        id: "craft-motion",
        name: "Motion design (GSAP)",
        tool: "motion_guide",
        summary: "Animated web sections, motion graphics and kinetic type with GSAP timelines, rendered frame-exact in Orvyn Studio.",
        description: "Teaches the agent motion design with GSAP: timelines, easing, choreography, kinetic typography and scroll-driven sections, plus the principles that make motion feel deliberate. Videos render frame-exact in Orvyn Studio (html-video or Reel); web deliverables ship as code. Unlocks the Web animation category on the marketplace (Pro hosting).",
        categories: &["web-animation", "motion", "explainer-video", "ui-build"],
        brief: "Motion craft: one gsap.timeline() per scene, defaults { ease: \"power3.out\", duration: 0.6 }; place steps with position parameters (\"<\", \"-=0.2\") so the scene reads as one gesture. Animate transform and opacity only. Lead with the most important element; stagger the rest 0.04–0.08 s. Hold the final state long enough to read (about 1 s per 10 words). Load GSAP from https://studio.local/lib/gsap/ in Studio; in html-video, time is virtual, so timelines render exactly.",
        sections: &[
            ("timing", "Durations: micro-interactions 0.15–0.3 s, element entrances 0.4–0.8 s, scene transitions 0.6–1.2 s, a full logo sting 2–4 s. Larger moves take longer. Exits are faster than entrances (about 70%). Leave 0.2–0.4 s of stillness between beats; constant motion is noise."),
            ("easing", "power2/power3.out for entrances (fast start, gentle landing), power2.in for exits, power2.inOut for moves between two places, expo.out for snappy UI, back.out(1.4) sparingly for playful pops, elastic never for serious brands. Use the same few eases across a piece so it feels like one hand made it. Springs in Reel: stiffness 120–200, damping 14–20 for UI."),
            ("choreography", "Decide the reading order and animate in that order: headline, then support, then actions. Overlap steps by 20–40% so nothing waits for the last thing to finish. Stagger lists from the side the eye starts at. Move things toward where they live (cards rise 16–40 px, they don't fly across the screen). Group related elements into one motion. One hero motion per scene; everything else supports it."),
            ("type", "Kinetic type: split into lines or words (GSAP SplitText), reveal with yPercent 100 → 0 inside an overflow-hidden mask, stagger 0.05 s. Keep text on screen long enough to read twice. Never animate letter-spacing or font-size (layout jank); scale the element instead. Avoid spinning or bouncing words unless the brief is playful."),
            ("scroll", "Web deliverables: ScrollTrigger with scrub: true for scenes tied to scroll, pin only one section at a time, set start/end with markers while building and remove them after. Respect prefers-reduced-motion: show the end state. Lazy-create timelines when a section nears the viewport. On phones, shorten distances and durations by about 30%."),
            ("video", "For video jobs: build the scene at the delivery size, make the timeline the source of truth (gsap.ticker driven by virtual time in html-video), pause it and seek(t) if you need window.seek. Title-safe margins 5% each side. Export 30 fps for UI motion, 60 fps for fast motion; check the first, middle and last frames, and any frame where something enters or exits."),
            ("principles", "Anticipation (a small move before a big one), follow-through (things settle, don't stop dead), squash and stretch only for cartoons, arcs for natural movement, and staging: the viewer should always know where to look. Motion explains change: use it to show where something came from or went, not as decoration."),
        ],
    },
    Craft {
        id: "craft-interface",
        name: "Interface design",
        tool: "interface_guide",
        summary: "Polished landing pages and app screens: type, spacing, colour, components and states, reviewed at phone and desktop widths.",
        description: "Teaches the agent to design and build interfaces that look made by a careful designer: a type scale, a spacing system, accessible colour, complete component states, purposeful effects, and a review pass that catches generic 'AI-made' patterns. Builds render and get checked in Orvyn Studio (html-still and design-review) at 375, 768 and 1440 px.",
        categories: &["ui-build", "design-review", "graphics", "web-animation"],
        brief: "Interface craft: pick one type family (two at most) and a scale (e.g. 14/16/20/28/40/56), a 4 px spacing grid, one accent colour plus neutrals, and stick to them. Body text 16 px+, line length 55–75 characters, contrast ≥ 4.5:1 (3:1 for large text). Every interactive element has hover, focus-visible, active and disabled states and a 44 px touch target. Build mobile first; no horizontal scroll at 375 px. Review your own screenshots at 375/768/1440 before delivering, and fix what you see.",
        sections: &[
            ("typography", "Hierarchy comes from size and weight, not colour. Headlines tight (line-height 1.0–1.15, letter-spacing slightly negative at large sizes), body 1.5–1.65. Use text-wrap: balance for headings and pretty for paragraphs. Numerals in data: tabular-nums. Limit weights to 2–3. Don't centre long text; don't set body copy in all caps."),
            ("layout", "Spacing from one scale (4, 8, 12, 16, 24, 32, 48, 64, 96). Related things closer together than unrelated things. A clear max width for text columns. Align to a grid; let one element break it on purpose. Sections need different rhythms (a dense feature grid, then a calm statement) so a page doesn't read as a stack of identical cards."),
            ("colour", "Neutrals do most of the work; the accent marks what's clickable or important, not decoration. Define tokens (background, surface, text, muted text, line, accent, accent text) for light and dark. Check contrast for every text/background pair, including text on the accent. Status colours (success, warning, danger) only for status."),
            ("components", "Buttons: one primary per view, clear labels (verbs), loading state that keeps width. Forms: visible labels (not placeholders), inline errors next to the field, helpful defaults. Empty states say what goes here and how to start. Loading: skeletons shaped like the content. Every list handles 0, 1 and many items, and long text."),
            ("effects", "Effects (glows, borders that trace, shimmering text, particles, marquees, number tickers) earn their place only when they point at something: one per page, near the main action or the key number. Prefer solid colour and real hierarchy over gradients and blur. Respect prefers-reduced-motion. Never let an effect lower legibility or contrast."),
            ("avoid", "Patterns that make work look generic: purple-to-blue gradients by default, emoji as icons or bullet points, three identical feature cards with an icon each, vague headlines ('Unlock the power of…'), stock phrases, centred everything, drop shadows on every element, inconsistent radii, placeholder copy left in. Write the real copy for the client's product; say concrete things."),
            ("review", "Render with design-review at 375, 768 and 1440 and look at each screenshot: hierarchy (what do you see first?), alignment, spacing consistency, contrast, tap targets, text overflow, broken images, sideways scroll. Fix, re-render, then deliver the source plus the final screenshots. Note anything you couldn't fix and why."),
        ],
    },
    Craft {
        id: "craft-blender",
        name: "Blender 3D",
        tool: "blender_guide",
        summary: "3D models, product renders and turntables made with headless Blender (bpy) on Orvyn Studio's GPU workers.",
        description: "Teaches the agent to model, light, render and export with Blender's Python API: clean topology from primitives and modifiers, materials, studio lighting, Cycles or EEVEE renders, turntables and game-ready GLB files. Runs headless on Orvyn Studio's GPU workers (Studio hosting).",
        categories: &["3d"],
        brief: "Blender craft: script everything in bpy from factory settings. Model in metres with real-world scale, apply transforms before export, name objects and materials. Light with three area lights or an HDRI-like world colour. Render with Cycles (64–256 samples + denoise) for stills, EEVEE for turntables. Export GLB with export_apply=True, check it loads and its size. Render previews from several angles and look at them before delivering.",
        sections: &[
            ("modelling", "Start from primitives (bpy.ops.mesh.primitive_*), shape with modifiers (Bevel for highlights on edges, Subdivision Surface for smooth forms, Solidify for thickness, Array/Mirror for repeats, Boolean sparingly), then apply them for export. Keep quads where possible. Set origin to the base for products. Shade smooth with auto-smooth (or Smooth by Angle) so hard edges stay sharp."),
            ("materials", "Principled BSDF for everything: base colour, roughness (0.2 glossy plastic, 0.5 satin, 0.85 matte), metallic 0 or 1. Use node textures (Noise, Voronoi) for subtle surface variation instead of flat colour. Keep materials few and named; GLB export keeps Principled values and image textures, not procedural nodes, so bake them if the GLB must match the render."),
            ("lighting", "Three-point with area lights: key (large, soft, 45° front-side), fill (half power, opposite), rim (behind, for edge separation). A seamless backdrop (a curved plane) for product shots. World colour low for contrast. Camera: 50–85 mm for products, depth of field off unless asked."),
            ("rendering", "Cycles: device GPU, samples 128 (stills) with OpenImageDenoise, adaptive sampling on; resolution within ORVYN_MAX_WIDTH/HEIGHT. EEVEE for quick previews and long turntables. Filmic or AgX view transform with medium contrast. Write stills with bpy.ops.render.render(write_still=True) into the output directory."),
            ("export", "GLB: select the deliverable objects, apply scale and rotation, export_scene.gltf(export_format='GLB', export_apply=True, export_yup=True). Check triangle count and file size against the brief; decimate if needed. Deliver the .glb, the .blend if asked, and preview renders from front, three-quarter and top."),
            ("turntable", "Parent the model to an empty, keyframe its Z rotation 0 → 360° over the frame range with linear interpolation (so the loop is seamless), 24–30 fps, 6–10 s. Render frames, then encode with the ffmpeg renderer (libx264, yuv420p) if an MP4 is needed."),
        ],
    },
    Craft {
        id: "craft-manim",
        name: "Math and chart animation (Manim)",
        tool: "manim_guide",
        summary: "Explainer animations of equations, graphs and charts with Manim Community, rendered in Orvyn Studio.",
        description: "Teaches the agent to explain ideas with Manim: equations that transform step by step, graphs and charts that build up, and pacing a viewer can follow. Renders in Orvyn Studio (Pro hosting).",
        categories: &["explainer-video", "motion"],
        brief: "Manim craft: one Scene per idea, construct() as a sequence of beats, each with self.play(...) then self.wait(0.5–1.5). Build equations with MathTex split into parts so TransformMatchingTex can move the parts that change. Use Axes/NumberPlane with labelled axes for graphs. Keep a consistent colour per concept across the whole video. Render at quality 'l' to check, then 'h' to deliver.",
        sections: &[
            ("structure", "Script the explanation first: one claim per beat, 3–6 beats per scene. Introduce, transform, conclude. Titles small and top-left; the subject centred. Clear the stage between ideas (FadeOut of a VGroup) instead of piling up objects."),
            ("equations", "MathTex(r\"a^2\", \"+\", r\"b^2\", \"=\", r\"c^2\") so each part is addressable; colour the terms that matter (set_color_by_tex). TransformMatchingTex between steps of a derivation; Indicate or Circumscribe to point at a term. Keep equations big enough to read on a phone (font_size ≥ 40 at 1080p)."),
            ("graphs", "Axes(x_range, y_range, axis_config={\"include_numbers\": True}), plot functions with axes.plot, label with axes.get_graph_label. Animate with Create for curves, a ValueTracker plus always_redraw for moving points and areas. For data charts, BarChart or build bars from Rectangles so they can grow from zero."),
            ("pacing", "Run times 1–2 s for most animations, waits after every important change. Total length to the brief; about 150 words of narration per minute if there's a voice-over. Add captions as text or deliver an .srt. Check the rendered video at normal speed before delivering."),
        ],
    },
    Craft {
        id: "craft-diagrams",
        name: "Diagrams (Rough.js and Excalidraw)",
        tool: "diagram_guide",
        summary: "Architecture, flow and concept diagrams, hand-drawn or clean, delivered as SVG, PNG and an editable Excalidraw file.",
        description: "Teaches the agent to draw diagrams that explain: the real structure, labelled connections, one idea per diagram, and a hand-drawn (Rough.js) or clean style. Delivered from Orvyn Studio as SVG, PNG and an editable Excalidraw file.",
        categories: &["diagrams", "graphics"],
        brief: "Diagram craft: decide the one question the diagram answers, then draw only what answers it. Lay out left-to-right or top-to-bottom along the flow; group related boxes in a labelled container; label every arrow with what moves along it. At most 7–9 boxes per diagram; split bigger systems into an overview plus details. Sketch style for explanations, clean style for documentation.",
        sections: &[
            ("structure", "Find the nouns (boxes) and the verbs (arrows). Put the main path on one line; side paths branch off it. Containers for boundaries (a service, a chain, a team). Keep 140 px+ between connected shapes so labels fit. Consistent sizes for things of the same kind."),
            ("style", "Sketch: roughness 1–1.5, hachure fills in light tints, a hand font for labels. Clean: no roughness, solid fills, a sans font. Use colour for meaning (one colour per kind of thing or per actor), never decoration; keep it to 3–4 colours plus neutrals. Dashed lines for optional or async paths."),
            ("labels", "Short labels (2–4 words) inside boxes; verbs on arrows ('signs', 'pays 10 USDT', 'emits event'). Number steps when order matters. A title and, if needed, a legend. Check it reads at the size it'll be shown (a slide, a tweet image, a doc)."),
            ("formats", "Deliver SVG (crisp, editable), PNG at 2x, and the .excalidraw file so the client can change it. For a set, keep the same style, sizes and colours across every diagram."),
        ],
    },
    Craft {
        id: "craft-video",
        name: "Video editing (FFmpeg)",
        tool: "video_guide",
        summary: "Cuts, joins, captions, audio and formats for the client's footage with FFmpeg in Orvyn Studio.",
        description: "Teaches the agent precise video editing with FFmpeg: probing inputs, frame-accurate trims, joins with matching formats, audio levels and fades, burned-in or sidecar captions, and delivery encodes for each platform. Runs in Orvyn Studio (Pro hosting).",
        categories: &["video-edit", "motion", "explainer-video"],
        brief: "Video craft: probe every input first (resolution, frame rate, audio rate, duration) and normalise them before joining. Trim with -ss after -i for frame accuracy. Keep audio in sync; fade audio in/out 0.2–0.5 s at cuts. Deliver H.264 (libx264, -crf 18–23, -pix_fmt yuv420p, -movflags +faststart) with AAC 128–192k unless the brief asks otherwise. Watch the start, every cut and the end of the render before delivering.",
        sections: &[
            ("probe", "Render a 2-second test first with the real settings. Check frame rate (-r), size (scale with -2 to keep the aspect ratio even), audio sample rate (-ar 48000) and channel layout. Different inputs: re-encode each to the same size/fps/audio before concat."),
            ("cutting", "Frame-accurate trim: -i in.mp4 -ss START -to END (re-encodes). Join same-format clips with the concat filter (filter_complex concat=n=N:v=1:a=1). Crossfades: xfade for video and acrossfade for audio, offsets computed from clip lengths."),
            ("audio", "Normalise loudness with loudnorm (I=-16 for social, -14 for music platforms). Duck music under speech with sidechaincompress, or lower it with volume. Fade with afade at the start and end. Never deliver clipping audio."),
            ("captions", "Burn in with drawtext (text=, font=, fontsize about 5% of height, a box behind for contrast, positioned in the lower third inside title-safe margins), or deliver .srt for platforms that support it. Keep lines under ~42 characters, 1–2 lines, 1–6 s each."),
            ("delivery", "Platform sizes: 1920x1080 landscape, 1080x1920 for Reels/Shorts/TikTok, 1080x1080 square. Keep bitrate sensible (CRF 20 is a good default), +faststart for the web. Give the file a clear name and state its length, size and format in the result."),
        ],
    },
];

pub fn craft(id: &str) -> Option<&'static Craft> {
    CRAFTS.iter().find(|c| c.id == id)
}

/// The craft packs that cover a job category.
pub fn crafts_for_category(category: &str) -> Vec<&'static Craft> {
    CRAFTS.iter().filter(|c| c.categories.contains(&category)).collect()
}

pub struct CraftPlugin {
    meta: PluginMeta,
    craft: &'static Craft,
}

pub fn crafts() -> Vec<Arc<dyn Plugin>> {
    CRAFTS
        .iter()
        .map(|c| Arc::new(CraftPlugin { meta: meta(c.id, c.name, "knowledge", c.summary, c.description, &["jobs"]), craft: c }) as Arc<dyn Plugin>)
        .collect()
}

#[async_trait]
impl Plugin for CraftPlugin {
    fn meta(&self) -> &PluginMeta {
        &self.meta
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        let c = self.craft;
        let cats: Vec<&str> = c.categories.iter().filter_map(|id| job_category(id).map(|j| j.name)).collect();
        let topics: Vec<&str> = c.sections.iter().map(|(t, _)| *t).collect();
        Some(format!("## Craft: {}\nFor {} jobs. {}\nRead {}(topic) for details before starting such a job (topics: {}).", c.name, cats.join(", "), c.brief, c.tool, topics.join(", ")))
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        let c = self.craft;
        let topics: Vec<&str> = c.sections.iter().map(|(t, _)| *t).collect();
        vec![Tool::new(
            c.tool,
            &format!("[Craft: {}] Detailed guidance on one topic.", c.name),
            json!({ "type": "object", "properties": { "topic": { "type": "string", "enum": topics } }, "required": ["topic"] }),
            move |args: Value| async move {
                let topic = args["topic"].as_str().unwrap_or("");
                Ok(Value::String(match c.sections.iter().find(|(t, _)| *t == topic) {
                    Some((t, text)) => format!("{} — {t}\n{text}", c.name),
                    None => format!("Topics: {}", c.sections.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", ")),
                }))
            },
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bliss::{Installed, Settings};
    use crate::tiers::Tier;

    #[test]
    fn hire_needs_the_craft_pack_for_its_category() {
        let hire = super::super::work::hire();
        let mut config = Settings::new();
        for (k, v) in [("category", "3d-web"), ("offerTitle", "A 3D product scene"), ("offerDescription", "An interactive three.js scene of your product, with stills and a turntable.")] {
            config.insert(k.into(), Value::String(v.into()));
        }
        let why = hire.validate(&config, &[], Tier::Pro).expect("refused without the pack");
        assert!(why.contains("3D for the web (three.js)"), "{why}");
        let pack = Installed { plugin: crafts().into_iter().find(|p| p.meta().id == "craft-threejs").unwrap(), config: Settings::new(), state: Value::Null, enabled: true };
        assert_eq!(hire.validate(&config, &[pack], Tier::Pro), None, "allowed with it");
        assert!(hire.validate(&config, &[], Tier::Basic).is_some(), "and still needs Pro hosting");
    }

    #[test]
    fn every_craft_is_well_formed() {
        let mut tools = std::collections::HashSet::new();
        for c in CRAFTS {
            assert!(c.id.starts_with("craft-") && tools.insert(c.tool), "{}", c.id);
            assert!(!c.sections.is_empty() && c.brief.len() < 700, "{} brief stays short", c.id);
            for cat in c.categories {
                assert!(job_category(cat).is_some(), "{} names unknown category {cat}", c.id);
            }
        }
    }
}
