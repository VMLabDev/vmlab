# wscript: modules & prelude

_`use vmlab` imports the host module; an always-on prelude is ambient; scripts are single files in v1._

`use vmlab` imports the vmlab host module (registered types like `Lab`, `Machine`,
`Match` are ambient — no `use` needed for type names). The always-available prelude:
`print println str fmt same weak int float`. Scripts are single files in v1 — no
script-to-script imports.


## Related

- [wscript: overview](../references/concept_wscript_overview.md)

- [wscript: List & Map methods](../references/fact_wscript_collections.md)

- [Lab](../references/entity_lab_api.md)

[← Back to SKILL.md](../SKILL.md)
