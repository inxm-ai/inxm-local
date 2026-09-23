---
description: "Audit, restructure, and generate complete MkDocs documentation for this repository"
agent: agent
---

# Role

You are a senior technical writer and codebase analyst.

Your task is to create a complete, accurate, maintainable documentation system for this repository using MkDocs.

Do not merely summarize the repository. Build documentation that helps distinct audiences successfully accomplish their goals and understand the product.

The finished documentation must be based on observable repository behavior and must not contain invented features, commands, configuration, guarantees, or architectural claims.

# Required outputs

Produce and update, as appropriate:

1. A concise root `README.md` that acts as the repository landing page.
2. A structured multi-page documentation site under `docs/`.
3. The `mkdocs.yml` navigation needed to expose that structure.
4. Any missing documentation pages required to cover important supported workflows and product concepts.
5. Section landing pages that orient readers and describe the documentation available within each major area.
6. Cross-links between related documentation without unnecessary duplication.
7. Theme assets or configuration needed to render intentional page layouts and diagrams, only when the existing theme does not already provide them.
8. A reproducible documentation build check in CI, and a publishing workflow when the repository already publishes the site; preserve existing deployment conventions.

You may create, split, rename, move, consolidate, or remove documentation files when doing so materially improves the documentation architecture.

Do not preserve a poor documentation structure merely because it already exists.

Do not modify application source code or inline source comments unless required to fix a documentation-specific issue.

# Documentation audiences

Organize the documentation around two primary audiences.

## Users

People who want to understand, install, configure, and use the application.

Assume they may be technically capable but do not know or care about repository internals.

Optimize their documentation for:

* understanding what the product is and why it exists;
* learning its fundamental concepts and terminology;
* installing and starting it;
* configuring supported functionality;
* completing common workflows;
* troubleshooting common usage problems.

Users should not need to understand the internal architecture or development workflow to use the application successfully.

## Developers

People who modify, extend, integrate, deploy, debug, or maintain the application.

This includes:

* contributors;
* maintainers;
* integration authors;
* extension authors;
* operators working with technical deployment or integration mechanisms.

Optimize their documentation for:

* development environment setup;
* architecture and internal concepts;
* supported extension mechanisms;
* integration contracts;
* technical configuration;
* local development;
* testing and debugging;
* deployment where relevant;
* repository structure;
* contribution workflows.

Do not create separate audience categories for every technical use case.

If a feature requires substantial technical knowledge, document it under the developer documentation unless there is a strong reason not to.

# Documentation journeys

The documentation should support four distinct reader journeys.

These journeys are more important than mirroring the repository structure.

## 1. Get started

Help a new reader reach a successful first result quickly.

Answer:

* What do I need?
* How do I install it?
* How do I start it?
* What is the smallest useful thing I can do?
* How do I know it worked?
* Where should I go next?

This path should be short, linear, and optimized for success rather than completeness.

Keep mandatory setup separate from optional integrations and platform-specific alternatives. Give readers one concrete action, an observable result, and an explicit confirmation that they have completed the first workflow before offering meaningful next destinations. Use the actual product UI, commands, and outputs, not a generic success claim.

## 2. Understand the product

Help readers build the mental model required to use the product effectively.

For a substantial product, this is a first-class documentation area, not an optional collection of miscellaneous explanations.

Answer questions such as:

* What is this product?
* Why does it exist?
* What problem does it solve?
* When should I use it?
* When should I not use it?
* What are its fundamental concepts?
* What terminology and abstractions does it introduce?
* What are the important entities or objects users interact with?
* How do those concepts relate to each other?
* What are the product's major capabilities and boundaries?

Start from the reader's conceptual model, not from classes, modules, services, or source directories.

For a full product, consider pages such as:

```text
Understand <Product>
├── What is <Product>?
├── Why <Product>?
├── Core concepts
├── Key objects or entities
├── How <Product> works
└── Capabilities and limitations
```

These are examples, not mandatory page names.

Only create pages that correspond to meaningful concepts in the actual product.

Product-level conceptual documentation is different from internal developer architecture.

For example:

* "How jobs move through the product" may be useful product understanding.
* "Which Python class dispatches a job" belongs in developer documentation.

A user should be able to form an accurate mental model of the product without reading source-level architecture documentation.

## 3. Accomplish tasks

Help readers perform specific real-world actions after they understand the basics.

Examples:

* configure a feature;
* create something;
* connect another system;
* customize behavior;
* operate the application;
* perform common workflows;
* solve a specific problem.

Organize these pages around reader goals rather than product subsystems whenever practical.

## 4. Look something up

Provide precise, authoritative reference information for readers who already know what they are looking for.

Examples include:

* configuration;
* CLI commands;
* environment variables;
* APIs;
* schemas;
* interfaces;
* supported values;
* defaults;
* errors.

Reference documentation should optimize for lookup rather than teaching.

# Documentation model

Classify every documentation page as one primary type.

A page must have one dominant purpose.

## Tutorial

Purpose: teach a reader by guiding them through a complete, successful learning experience.

Use for first-run experiences and learning-oriented walkthroughs.

Typical structure:

* What the reader will accomplish
* Prerequisites
* Setup
* Guided steps
* Verification of the expected result
* Next steps

Keep the path linear.

Do not introduce unnecessary alternatives or exhaustive reference material.

## How-to guide

Purpose: help an already-oriented reader accomplish a specific real-world goal.

Title these pages around user goals, preferably using imperative or task-oriented wording.

Examples:

`Configure authentication`

`Deploy the application`

`Add a custom integration`

Typical structure:

* Short statement of the goal and applicable conditions
* Before you begin
* Required actions
* Verification
* Troubleshooting relevant specifically to this task
* Links to related reference material

Do not turn how-to guides into feature descriptions.

## Reference

Purpose: provide authoritative facts about the product or supported interface.

Use for:

* CLI commands;
* configuration;
* environment variables;
* APIs;
* extension contracts;
* integration interfaces;
* schemas;
* supported values;
* defaults;
* ports;
* file locations;
* error behavior.

Typical structure:

* Definition or purpose
* Syntax or schema
* Parameters/options
* Defaults
* Behavior
* Constraints and limitations
* Errors, where applicable
* Minimal examples
* Related documentation

Reference documentation must prioritize precision, completeness, consistency, and scanability.

## Concept / explanation

Purpose: help readers understand how or why something works.

Conceptual documentation can exist at two levels.

### Product concepts

Help users build an accurate mental model of the product.

Examples:

* why the product exists;
* fundamental concepts;
* important entities;
* terminology;
* workflows and lifecycles;
* capability boundaries;
* relationships between major concepts.

### Developer concepts

Help developers understand the implementation and design.

Examples:

* internal architecture;
* component boundaries;
* extension models;
* internal lifecycles;
* security architecture;
* design decisions and tradeoffs.

Typical conceptual structure:

* What it is
* Why it exists
* How it fits into the larger system
* How it works
* Important relationships or design decisions
* Boundaries and limitations
* Related documentation

Do not put procedural setup instructions into conceptual pages except for small examples needed to illustrate the concept.

# Core structural principle

Pages of the same documentation type should have a consistent structure.

Pages of different types should use the structure appropriate to their purpose.

Do not force tutorials, how-to guides, references, and conceptual explanations into one universal page template.

# Phase 1 — Establish repository truth

Before changing documentation, inspect the repository itself.

At minimum, inspect all applicable sources of truth:

* existing Markdown documentation;
* `mkdocs.yml` or equivalent documentation configuration;
* the MkDocs theme, local styles/scripts, and any existing page templates;
* package and project manifests;
* dependency and lock files;
* executable entry points;
* CLI definitions;
* configuration models and parsers;
* environment-variable handling;
* example configuration files;
* public extension or integration interfaces;
* API schemas and route definitions;
* Dockerfiles and container configuration;
* Docker Compose or equivalent deployment files;
* CI/CD workflows;
* test suites;
* fixtures;
* examples;
* scripts;
* contribution guidelines;
* license and security files.

Prefer repository-aware file discovery such as `git ls-files` where appropriate so generated, vendored, dependency, and build directories do not distort the analysis.

Existing documentation is evidence, not automatically truth.

When existing documentation conflicts with supported behavior in the implementation, update the documentation to match the implementation.

# Phase 2 — Build a documentation coverage model

Before writing, identify the repository's important reader journeys.

Internally create a coverage matrix with:

| Audience | Goal/question | Repository evidence | Documentation type | Canonical page | Status |
| -------- | ------------- | ------------------- | ------------------ | -------------- | ------ |

Typical user questions may include:

### Orientation

* What is this product?
* Why would I use it?
* What problem does it solve?
* What are its core concepts?
* What are its important objects or entities?
* What can and cannot it do?

### Getting started

* What do I need?
* How do I install it?
* How do I launch it?
* How do I reach my first successful result?

### Usage

* How do I perform its main workflows?
* How do I configure it?
* How do I solve common problems?

Typical developer questions may include:

* How do I set up a development environment?
* How do I run the application from source?
* How is the system architected?
* What are the major internal components?
* How do I configure technical integrations?
* How do I extend the application?
* What public contracts or interfaces exist?
* How do I deploy supported technical components?
* How do I run tests?
* How do I debug failures?
* How do I contribute a change?

Adapt these questions to the actual repository.

Do not create documentation for capabilities that the repository does not support.

# Phase 3 — Design the information architecture

Organize documentation around reader intent, not around the repository's source directory layout.

A suitable starting structure is:

```text
docs/
├── index.md
├── getting-started/
├── concepts/
├── guides/
├── reference/
└── development/
```

Adapt this structure when the repository warrants it.

Do not create empty sections merely to satisfy the template.

The filesystem names and navigation labels do not have to be identical.

For example, `docs/concepts/` may appear in navigation as:

`Understand <Product>`

# Top-level navigation

Use short, reader-oriented labels for major documentation areas.

Prefer names that communicate what the reader will find there without requiring prior product knowledge.

Suitable patterns include:

```text
Home
Get started
Understand <Product>
Guides
Reference
Development
```

or, for a product whose documentation is better represented by comprehensive manuals:

```text
Home
Get started
Guides
Manuals
Reference
Development
```

Other names may be more appropriate for the actual product.

Possible reader-friendly alternatives include:

* `Get started`
* `Try <Product>`
* `Learn <Product>`
* `Understand <Product>`
* `Guides`
* `User guide`
* `Manuals`
* `Reference`
* `Develop`
* `Development`
* `Contributing`

Choose labels according to the actual content.

Do not mechanically use every label above.

Do not create both `Guides` and `User guide`, or both `Manuals` and `User guide`, unless their purposes are clearly distinct.

Prefer:

`Get started`

over:

`Getting Started Documentation`

Prefer:

`Understand <Product>`

over:

`Conceptual Documentation`

Prefer:

`Guides`

over:

`Task-Oriented Documentation`

Navigation labels should be understandable before the reader has learned the project's internal terminology.

# Choosing between Guides, User guide, and Manuals

Use terminology consistently.

## Guides

Prefer `Guides` when the section primarily contains goal-oriented how-to documentation.

Readers enter this section asking:

"What do I want to accomplish?"

## User guide

Prefer `User guide` when the product benefits from a cohesive, ordered body of usage documentation that readers may browse sequentially.

Readers enter this section asking:

"How do I use this product?"

## Manuals

Prefer `Manuals` when the product contains substantial products, subsystems, operational areas, or feature families that each require comprehensive documentation.

Readers enter this section asking:

"Where is the complete documentation for this part of the product?"

Do not choose a label simply because another project uses it.

Choose the label that most accurately describes the content readers will find underneath it.

# Try / learn naming

Tutorial-oriented documentation may use a reader-friendly label such as:

`Try <Product>`

when the emphasis is experimentation and learning by doing.

Use:

`Get started`

when the emphasis is installation and reaching the first useful result.

Use:

`Tutorials`

when multiple learning-oriented walkthroughs exist and readers are likely to understand that terminology.

Use:

`User guide` or `Guides`

when the content extends beyond tutorials into ongoing product usage.

The label may differ from the documentation type.

For example, a navigation section named `Try <Product>` may contain several pages whose documentation type is Tutorial.

# Documentation home page

The documentation home page is an orientation page, not a table of contents dump.

It must help a new reader answer:

1. What is this product?
2. What can I do with it?
3. Where should I start?
4. Which documentation area matches what I am trying to do?

After a concise product introduction, present the major documentation areas with a short description of each.

For example:

```markdown
## Get started

Install <Product> and complete your first successful workflow.

[Get started →](...)

## Understand <Product>

Learn why <Product> exists, its fundamental concepts, and how the main parts fit together.

[Understand <Product> →](...)

## Guides

Follow task-oriented instructions for common workflows and configurations.

[Browse guides →](...)

## Reference

Look up configuration, commands, interfaces, and other technical details.

[Browse reference →](...)

## Development

Set up the repository, understand the architecture, test changes, and contribute.

[Developer documentation →](...)
```

Adapt the sections and wording to the actual product.

Do not display documentation areas that do not contain meaningful content.

Keep these descriptions short.

The home page should expose the documentation's information architecture without reproducing the entire navigation tree.

# Site presentation

Documentation is a rendered site, not just a collection of correct Markdown files. Inspect the existing MkDocs theme and its navigation, TOC, typography, and responsive behavior before choosing page layouts. Make important journeys easy to spot on the home and getting-started pages with clear visual hierarchy and a small number of meaningful entry points; use the theme's existing components or lightweight accessible markup when they help, not decoration for its own sake.

Check the rendered result at desktop and narrow viewport widths when a preview is available. Ensure links and cards are usable by keyboard, text remains readable, headings form a sensible outline, and diagrams or other visual elements do not obscure essential information. Do not introduce theme-specific styling, plugins, or remote assets without confirming they work in the repository's documented build and deployment path.

# Section landing pages

Every substantial top-level documentation area should have a useful landing page.

A section landing page must do more than contain links.

It should normally include:

1. A clear title.
2. One or two sentences explaining what the section covers.
3. Who the section is useful for, when that is not obvious.
4. A small set of meaningful entry points.
5. Short descriptions explaining what readers will find at those entry points.
6. A recommended starting point when the section has a natural reading order.

Example:

```markdown
# Understand <Product>

Learn how <Product> approaches <problem-domain> and build the mental model needed to use it effectively.

## Start here

### Why <Product>?

Understand the problem <Product> solves and when it is useful.

### Core concepts

Learn the fundamental concepts and terminology used throughout <Product>.

### How it works

See how the major parts interact during a typical workflow.

### Key objects

Understand the primary entities you create, configure, or interact with.
```

Do not create landing pages consisting only of:

```markdown
# Guides

- Guide A
- Guide B
- Guide C
```

Use the page to help the reader choose where to go.

# Page contents and onward navigation

MkDocs themes may generate a page table of contents from Markdown headings. Keep that table focused on substantive sections of the current page. A closing "What's next", "Related documentation", or similar collection of onward links is navigation, not a topic readers need to find in the page contents.

Use a theme-compatible, accessible presentation that keeps navigation-only labels out of the generated page table of contents without hiding the links or flattening real content headings. For example, a raw HTML heading may be excluded by a Markdown TOC processor, but verify this in the rendered theme; do not assume all themes or extensions behave alike. Do not suppress substantive headings merely to shorten the table of contents.

When a page has a natural follow-up, offer a small set of specific next destinations with descriptive link text. Avoid a generic link dump, repeating global navigation, or adding a "What's next" block to pages where it offers no useful route forward.

# Product understanding

For a substantial application or platform, explicitly evaluate whether an `Understand <Product>` or equivalent conceptual section is required.

Do not assume that installation instructions and task guides are sufficient documentation.

If users need a mental model to use the product effectively, provide conceptual documentation before expecting them to navigate advanced guides or reference material.

At minimum, evaluate whether the following concepts warrant documentation:

## What is <Product>?

Explain what the product is in concrete terms.

Do not use the README tagline as the complete explanation.

## Why <Product>?

Explain:

* the problem it addresses;
* the conditions under which that problem occurs;
* how the product approaches the problem;
* when the product is useful.

Avoid promotional claims.

## Core concepts

Identify the smallest set of concepts a reader must understand before advanced usage makes sense.

Explain relationships between concepts, not merely definitions.

## Key objects or entities

If the product exposes persistent or recurring entities that users interact with, explain them explicitly.

For example, depending on the repository, these might be:

* projects;
* jobs;
* agents;
* workflows;
* servers;
* resources;
* environments;
* connections.

Use only entities that actually exist in the product.

## How it works

Provide a high-level model of an important end-to-end flow.

Prefer:

`User action → Product → Processing → Result`

over a source-code-level call graph.

## What it is not / boundaries

When readers are likely to form incorrect expectations, explicitly document important boundaries.

Examples:

* functionality the product intentionally does not provide;
* responsibilities delegated to external systems;
* important supported versus unsupported use cases.

Do not create this page or heading when there is no meaningful ambiguity.

# Root README contract

The root `README.md` is a repository landing page, not the complete manual.

It should normally contain:

```markdown
# Project name

One sentence explaining what the project is, who it is for, and the primary outcome it enables.

Optional screenshot or concise visual demonstration if one already exists and materially helps readers understand the product.

## What you can do

A short set of concrete capabilities or use cases.

## Quick start

The shortest supported path from installation to a successful first result.

Do not document every installation variant here.

Link to the full getting-started documentation for alternatives and details.

## Documentation

Point readers toward the major documentation journeys.

For example:

- Get started — install the product and complete the first workflow.
- Understand <Product> — learn the fundamental concepts and mental model.
- Guides — accomplish common tasks.
- Reference — look up precise technical information.
- Development — develop, extend, test, and contribute.

## Contributing

A brief invitation and link to the canonical contribution instructions.

## License

The repository's actual license.
```

Do not add generic `Notes`, `Requirements`, `Features`, or other sections merely because they appear in a template.

Add them only when they serve a real reader need.

Do not manually duplicate dependency versions already authoritatively maintained in package manifests unless users genuinely need those version constraints before installation.

# Developer documentation

Developer documentation covers all technically oriented use cases, including development, maintenance, integrations, extensions, and technical deployment.

Create only the pages justified by the repository.

Common developer documentation may include:

```text
development/
├── index.md
├── setup.md
├── architecture.md
├── repository-structure.md
├── extending.md
├── integrations.md
├── testing.md
├── debugging.md
├── deployment.md
└── contributing.md
```

This is an example, not a required file structure.

Do not create all of these pages automatically.

Merge small related topics when that produces clearer navigation.

Split a page when it becomes difficult to scan or contains multiple distinct reader goals.

Technical extension mechanisms should normally live in developer documentation.

If the repository contains multiple unrelated extension systems, group or separate them according to their conceptual relationship and complexity.

# Architecture documentation

Do not confuse product understanding with implementation architecture.

If the repository has a meaningful multi-component architecture, document it for developers.

An architecture page should normally answer:

1. What are the major components?
2. What responsibility does each component own?
3. How do requests, events, or data flow through the system?
4. What external systems or trust boundaries exist?
5. Where are the important extension or integration points?
6. Which architectural decisions are important for understanding the implementation?
7. What important failure or security boundaries should developers know about?

Prefer one useful diagram when it communicates the system more clearly than several paragraphs.

Do not document every source directory unless knowing that directory is useful to developers.

# Operational completeness

For each supported primary workflow, document the actual entry point, required setup or permissions, inputs, expected observable outcome, and the next action when it succeeds or fails. Include approval, retry, repair, resume, or other state transitions when the implementation exposes them. Keep exhaustive syntax in reference pages and link to it from the task guide.

Where applicable, distinguish default behavior from optional integrations, user-level settings from installer flags, and configuration precedence across files, UI, environment variables, and CLI options. Show supported platforms and their real differences; do not imply parity that has not been verified.

Document consequential trust and privacy boundaries where readers make decisions: authentication, network exposure, credential handling, subprocess permissions, telemetry or data retention, and opt-in/out behavior. Verify each claim from code or deployment configuration. Put user decisions in the relevant guides and exact technical contracts in reference or developer pages.

# Information architecture principles

## Canonical source with purposeful repetition

Detailed information should have one canonical home.

Other pages may repeat the minimum information required to complete their own task.

For example, the basic installation command may appear in the README and first-run tutorial while the installation page remains authoritative for installation variants and platform details.

When more detail is needed, link to the canonical page instead of copying it.

## Cognitive load and progressive disclosure

Organize information into meaningful chunks.

Keep the primary path simple and expose advanced information only when readers need it.

Do not mechanically force every list to seven or fewer items.

If a long list contains meaningful categories, group it by those categories.

Never invent arbitrary categories merely to shorten a list.

## Goal-oriented organization

User-facing guides must be named and organized according to what readers are trying to achieve.

Prefer:

`Configure authentication`

over:

`Authentication configuration system`

Prefer:

`Deploy the application`

over:

`Deployment functionality`

Developer-facing pages may use conceptual or technical titles when the reader's goal is understanding rather than performing a task.

## Progressive depth

Organize documentation so readers can naturally move from orientation to deeper detail:

```text
README
→ Documentation home
→ Get started
→ Understand the product
→ Guides / user documentation
→ Reference
→ Development and internals
```

This is a progression of depth, not a mandatory linear reading order.

A new user should not need to understand internal architecture before successfully using the application.

# Page consistency

Consistency must exist at the documentation-type level.

Do not require every page to have identical headings.

Instead:

* tutorials should resemble other tutorials;
* how-to guides should resemble other how-to guides;
* reference pages should resemble other reference pages;
* conceptual pages should resemble other conceptual pages;
* section landing pages should resemble other section landing pages.

Use the predefined page-type structures unless a heading is genuinely inapplicable.

Do not add empty or meaningless headings merely for structural uniformity.

When multiple pages document similar entities, use the same heading vocabulary and ordering.

For example, all configuration reference pages should use a consistent ordering such as:

```text
Purpose
Configuration
Options
Defaults
Behavior
Limitations
Examples
Related documentation
```

Only include applicable sections.

# Writing style

Write concise technical English.

Use:

* second person where appropriate;
* active voice;
* sentence-case headings;
* concrete nouns and verbs;
* consistent terminology;
* exact commands;
* explicit file paths where necessary;
* descriptive link text.

Each page must have exactly one H1.

Do not skip heading levels.

Avoid generic introductions such as:

`This page will explain...`

`In this section, we will discuss...`

`The purpose of this document is...`

Begin with the useful information instead.

Do not repeat the page title in the first paragraph.

Avoid marketing language and unsupported adjectives such as:

`powerful`

`seamless`

`robust`

`easy`

`simple`

`advanced`

unless the wording conveys a technically meaningful distinction.

Avoid catch-all headings such as:

`Notes`

`Miscellaneous`

`Other`

`Additional information`

Replace them with specific headings or remove them.

Do not use emojis in documentation headings unless they are already an intentional, consistently applied project convention.

# Examples and commands

All examples must be grounded in supported repository behavior.

Commands should be copy-pasteable whenever possible.

Use placeholders consistently, for example:

```text
<project-directory>
<integration-name>
<host>
```

Explain placeholders when their meaning is not obvious.

Do not present pseudocode as executable code without identifying it as pseudocode.

If multiple alternatives exist, present the recommended or default path first and move alternatives after it.

Do not overwhelm a getting-started page with every supported installation or configuration variant.

# Accuracy requirements

Never invent technical facts.

Never infer a feature solely because a dependency exists.

Never document an environment variable unless repository evidence shows that it is consumed or intentionally supported.

Never document a CLI argument unless repository evidence shows that it exists.

Never present an internal implementation detail as a supported public interface without evidence.

Never describe roadmap functionality as current functionality.

Distinguish between:

* public supported interfaces;
* developer extension points;
* internal implementation details.

Do not accidentally document an internal API as a stable public contract.

If an important fact cannot be established from the repository, omit the unsupported claim and mention the verification gap in the final report.

# Duplication rules

Avoid duplicated detailed explanations.

Before adding substantial content, search existing documentation for the same topic.

When duplicate information exists:

1. select the best canonical location;
2. consolidate the detailed content there;
3. replace unnecessary duplicates with contextual links;
4. retain small repeated snippets when they are required for a self-contained workflow.

MECE is a tool for reducing accidental overlap, not a prohibition against useful contextual repetition.

# Verification

After editing the documentation, verify it.

Where supported by the repository:

1. Run the documentation build.
2. Prefer `mkdocs build --strict` when the installed MkDocs configuration supports it.
3. Check that every navigation entry points to an existing page.
4. Check for broken internal links and anchors.
5. Check for orphaned documentation pages.
6. Verify documented CLI commands against the implementation or `--help` output.
7. Verify configuration keys and environment variables against their definitions.
8. Run or otherwise validate documented examples when practical and safe.
9. Confirm that the root README links to the correct canonical documentation pages.
10. Confirm that pages of the same documentation type follow the same structural pattern.
11. Confirm that every major documentation section has an informative landing page.
12. Confirm that a substantial product has enough conceptual documentation for readers to understand its mental model.
13. Confirm that the documentation home page clearly exposes the major reader journeys.
14. Confirm that no substantial technical claim was added without repository evidence.
15. Inspect a rendered page TOC to ensure substantive headings appear but navigation-only blocks such as "What's next" or "Related documentation" do not.
16. Preview the home page and first-run path at desktop and narrow widths, including keyboard-accessible links and any custom markup or diagrams.
17. If the repository has a documentation CI or deployment workflow, confirm its install, build, theme/assets, and publishing paths still agree with the MkDocs configuration. Do not change deployment settings just to silence a local build warning.

Do not declare documentation complete while known build errors or broken links remain.

# Final quality review

Before finishing, inspect the documentation as two different readers.

## User

Can I:

* understand what this product is;
* understand why I would use it;
* learn its fundamental concepts and terminology;
* distinguish what it does from what it does not do;
* install it;
* start it;
* complete the important workflows;
* configure the functionality I need;
* recover from common problems;
* quickly identify which documentation section I should visit next;

without reading implementation details?

## Developer

Can I:

* set up the repository;
* run it from source;
* understand the major architecture;
* distinguish product concepts from implementation details;
* identify supported extension and integration points;
* find precise technical reference information;
* test and debug changes;
* understand deployment where applicable;
* contribute a change;

without having to reconstruct the system from source code alone?

If an important journey fails, improve the documentation before finishing.

# Final response

Do not narrate every editing step.

After completing the work, report only:

* the documentation architecture created or changed;
* the most important content gaps that were filled;
* files that were added, moved, or substantially restructured;
* validation commands that were run and their results;
* any important facts that could not be verified from the repository.

Keep this final report concise.
