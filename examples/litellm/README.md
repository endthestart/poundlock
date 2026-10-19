# Optional LiteLLM evaluation stack

This example starts LiteLLM with PostgreSQL so Poundlock can use separate
application identities and budgets. It is optional. Connect an existing LiteLLM
deployment when one is already operated by your organization.

1. Copy `.env.example` to `.env`. Replace every placeholder with independent
   random values and reviewed immutable image digests. The LiteLLM master key
   must start with `sk-`.
2. Keep `LITELLM_LISTEN_ADDRESS=127.0.0.1` for local evaluation. If Poundlock is
   on another machine, bind a private interface or put the gateway behind an
   authenticated TLS reverse proxy. Do not expose the administrative key.
3. Run `docker compose up -d` and wait for both services to become healthy.
4. Configure provider routes and organization limits in LiteLLM. Poundlock does
   not own those settings.
5. In Poundlock Setup, add `http://HOST:4000/v1` as a LiteLLM endpoint. Choose
   **Separate application identities and budgets**, enter the LiteLLM master key,
   set a maximum spend and reset period, and save. Poundlock validates model
   listing and key-management access.

Back up the PostgreSQL volume and LiteLLM configuration according to LiteLLM's
current documentation. Upgrade LiteLLM independently and re-run the Poundlock
endpoint test before changing the pinned image. Remove the example with `docker
compose down`; add `--volumes` only when you deliberately intend to delete its
database.

See LiteLLM's [Docker quick start](https://docs.litellm.ai/docs/proxy/docker_quick_start)
and [virtual-key setup](https://docs.litellm.ai/docs/proxy/virtual_keys) for the
version you selected.
