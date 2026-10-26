# Documentation guidelines

Choose a section by the reader's goal:

- **User Guide:** Task-oriented instructions for using the application to accomplish a goal. Explain what to do and the expected outcome.
- **Tutorials:** Guided, hands-on learning through a concrete example. Lead readers through a complete exercise with an observable result.
- **Reference:** Precise descriptions of supported application behavior, rules, interfaces, configuration, and constraints. Technical runtime behavior belongs here when readers need to look up how the app works.
- **Developer Guide:** Guidance for building, testing, contributing to, or extending the application, including internal architecture and implementation decisions.

Keep each topic in its canonical section and link to it from other sections instead of duplicating the explanation.

This file is agent guidance, not published documentation. Keep it excluded through `exclude_docs` in `../mkdocs.yml` and out of its navigation, which also controls the in-app documentation bundle.
