# Browser wake-path smoke test

This nested package verifies the real shared-memory Wasm GPUI bridge, not just
Wasm compilation. A browser `setTimeout` changes a Hyphae cell after startup;
the adapter must wake GPUI without polling. Success changes the document title
to `hyphae-gpui: PASS` and logs `hyphae-gpui bridge received 42`.

```sh
env -u NO_COLOR trunk serve
```

The Trunk configuration supplies the COOP/COEP headers and shared-memory Wasm
flags required by GPUI. Open <http://127.0.0.1:8084> and check the title or
browser console after startup.
