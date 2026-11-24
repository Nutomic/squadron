use crate::schema::Variant;

const ANGLES: [&str; 10] = [
    "retro arcade cabinet",
    "minimal productivity tool",
    "satire of enterprise software",
    "cozy low-stakes simulation",
    "brutalist art experiment",
    "1990s web nostalgia",
    "tiny interactive explainer",
    "toy operating system desktop",
    "single-page game with score chase",
    "playful postcard from the future",
];

const STACKS: [&str; 10] = [
    "vanilla HTML/CSS/JS",
    "TypeScript with Vite",
    "React with Vite",
    "Svelte with Vite",
    "canvas 2D and vanilla JS",
    "WebGL shader background",
    "SVG animation and CSS",
    "Web Components",
    "Alpine.js plus CSS grid",
    "canvas particles and requestAnimationFrame",
];

const TONES: [&str; 10] = [
    "playful",
    "restrained",
    "absurd",
    "warm",
    "stark",
    "nostalgic",
    "instructional",
    "wry",
    "energetic",
    "dreamlike",
];

const CONSTRAINTS: [&str; 10] = [
    "no external services or network calls",
    "fully self-contained in one workdir",
    "serve on port 8080 bound to 0.0.0.0 (all interfaces; never 127.0.0.1)",
    "no secrets or credentials in code",
    "works offline after load",
    "keyboard and pointer input both supported",
    "responsive down to 375px",
    "no third-party fonts or images",
    "all state local to the page",
    "clear visible reset control",
];

pub fn template_variants(n: usize) -> Vec<Variant> {
    (0..n)
        .map(|index| {
            let slot = index % 10;
            Variant {
                angle: format!("{} (interpretation {})", ANGLES[slot], index + 1),
                stack: STACKS[(slot + index / 10 + 1) % 10].to_owned(),
                tone: TONES[(slot * 3 + index / 10) % 10].to_owned(),
                constraints: vec![
                    CONSTRAINTS[slot].to_owned(),
                    CONSTRAINTS[(slot + 4) % 10].to_owned(),
                    format!("distinct from the other {} variants", n.saturating_sub(1)),
                ],
            }
        })
        .collect()
}

/// User-visible case text for the template fallback: the original prompt
/// appears exactly once, followed by this case's distinct angle (with its
/// stack/tone). No boilerplate constraints — those are appended at execution
/// time, and no second copy of the prompt.
pub fn template_case_prompt(prompt: &str, variant: &Variant) -> String {
    format!(
        "{prompt}\n\nAngle: {angle}. Stack: {stack}. Tone: {tone}.",
        angle = variant.angle,
        stack = variant.stack,
        tone = variant.tone
    )
}
