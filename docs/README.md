# ZOE

ZOE, the Zope Object Engine, is a ZODB storage written in Rust. Plone 6.1 and later use it unchanged: in-process for one Zope instance, or behind a server that speaks the ZEO protocol for a cluster.

Nothing is usable yet. The project is in phase 0 of the roadmap.

- The [technical specification](spec/technical-specification.md) says what ZOE is and how it works.
- The [working methods and roadmap](spec/working-methods.md) say how ZOE gets built.
- The [architecture decision records](adr/README.md) record each decision with its reasons.
- The [glossary](glossary.md) gives the vocabulary.

The source is at [github.com/adeweb-be/ZOE](https://github.com/adeweb-be/ZOE), under the Apache-2.0 licence.
