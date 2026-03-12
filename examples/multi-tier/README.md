# multi-tier

A multi-group infrastructure setup using plan includes for shared configuration.

## What It Does

1. Includes a shared `common/base.kdl` plan that installs base packages and enables NTP
2. Installs and starts nginx on the web tier

This example demonstrates how to use `include` to share common configuration across multiple plans.

## Structure

```
multi-tier/
├── inventory.kdl       # Web, app, and db groups
├── plan.kdl            # Main plan (targets web, includes base)
├── common/
│   └── base.kdl        # Shared base configuration
└── README.md
```

## Usage

```bash
glidesh run -i examples/multi-tier/inventory.kdl -p examples/multi-tier/plan.kdl
```
