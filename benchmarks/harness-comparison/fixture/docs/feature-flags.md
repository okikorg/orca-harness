# Feature flag precedence

Resolve feature flags from lowest to highest precedence:

1. `config/features.yaml` defaults
2. the active environment file, such as `config/runtime.yaml` in production
3. the active region file, such as `config/region-eu.yaml`

Values absent from a higher-precedence file retain the lower-precedence value.
