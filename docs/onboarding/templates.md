# Onboarding templates per industry

A new plant should start from a template, not a blank sheet. A template is a
starter manifest in the same format as
[`test-org-e2e-manifest.json`](test-org-e2e-manifest.json), kept in
`docs/onboarding/templates/` and compiled into the `onboard_template` binary.

```bash
cargo run --bin onboard_template -- --list
cargo run --bin onboard_template -- --template building-materials \
    --org "Acme Blocks" --plant "Plant 1" --out onboarding-data/acme.json
```

The output is validated before it is written: every stock row names a declared
godown and gives exactly one of `size_mm` or `volume_in_size`; vehicle
documents use one of the five compliance types.

## building-materials

- **Block sizes:** AAC blocks 600x200 mm in 100, 150, 200, 250 and 300 mm
  thickness, 160 pieces per pallet, at quantity 0. Delete the sizes the plant
  does not make; add others with `size_mm`.
- **Litre convention:** everything is litres. A piece's volume is
  `l x w x h` in mm divided by 1,000,000 (600x200x150 = 18 L), the same rule as
  [stock-categories.md](stock-categories.md). Vehicle `capacity` is in litres.
- **Compliance docs:** vehicles take the five papers (`RegistrationCertificate`,
  `Insurance`, `PollutionCertificate`, `FitnessCertificate`, `Permit`); see
  [vehicle-compliance.md](vehicle-compliance.md).
- Fill in the `REPLACE ME` address and coordinates, then add drivers, vehicles,
  customers and users.

## Adding an industry

Add `docs/onboarding/templates/<name>.json` and one line in `TEMPLATES` in
`src/logistics/onboarding/mod.rs`; the unit test renders and validates every
template.

## Not yet built

The `onboard_org` loader that applies a manifest to the API is referenced in
[test-org-e2e.md](test-org-e2e.md) but is not in this repo.
