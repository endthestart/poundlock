# Poundlock ecosystem compatibility

Poundlock, [Poundlock Dispatch](https://github.com/endthestart/poundlock-dispatch) and
[Poundlock Toolkit](https://github.com/endthestart/poundlock-toolkit) are separate
products with their own repositories, versions and releases. They depend on each
other only through versioned documents, never through shared branches, submodules
or another repository's checkout.

- **Toolkit** runs before Poundlock exists. It measures a machine and model setup
  and exports evidence. `recommend --format json` writes the versioned summary
  (`urn:poundlock-toolkit:recommendation:1`) that Poundlock's
  `scripts/toolkit-capabilities.py` turns into capability evidence.
- **Poundlock** owns inference discovery, admission, runtime readiness, managed
  external access, usage and durable jobs. Its contract is `/api/openapi.yaml`.
  A client calls `GET /api/v1/capabilities` and checks `api_major`,
  `contract_revision` and the feature names it needs.
- **Dispatch** owns projects, planning, worker orchestration, review and the
  desktop and terminal clients. It declares the Poundlock contract it needs in
  `compat/poundlock.json` and refuses an older controller with a message that
  says what to upgrade.

Fields are added, never repurposed. Clients keep unknown values and report a clear
incompatibility when a required feature or revision is missing.

## Tested combination

| Release | Toolkit | Poundlock | Dispatch |
|---|---|---|---|
| Poundlock 0.3.0 | 0.5.0 (capability summary) | 0.3.0, API 1, contract revision 5 | 0.3.0 |

Each product's changelog records the combination its release was tested with.

## Changing a contract

1. Change the owning schema and its fixture, additively.
2. Release the producer.
3. Update consumers and their fixtures, and raise a consumer's minimum only when it
   needs the new feature.
