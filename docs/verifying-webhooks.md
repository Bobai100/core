# Verifying webhook signatures

When a destination has a `webhook_secret`, every TxWatch webhook POST carries:

```
X-TxWatch-Signature: sha256=<lowercase hex HMAC-SHA256 of the request body, keyed with the secret>
```

To verify a request, a receiver must:

1. **Use the raw request body bytes.** Compute the HMAC over the bytes exactly as
   received, before any JSON parsing. Re-serialising a parsed body changes key
   order, whitespace or number formatting and breaks the signature.
2. **Compare in constant time.** Use your platform's constant-time comparison
   (shown below), never `==` on strings, so the comparison time does not leak
   how many leading bytes matched.
3. **Reject** a missing header, a header without the `sha256=` prefix, invalid
   hex, or a mismatch, with `401` and without processing the body.

## Test vector

Use this to check your implementation. It is also checked by the Rust test
suite (`signature_matches_documented_test_vector` in `crates/notifier`).

| Field     | Value                                                                     |
|-----------|---------------------------------------------------------------------------|
| Secret    | `whsec_test_vector`                                                       |
| Body      | `{"alert_id":"test-vector","amount":"100.0000000"}` (no trailing newline) |
| Signature | `sha256=0330f633146e241611d9308f5c7d38cb97ad862c0a2a22895889208691a9d691` |

## Rust

Using the `hmac`, `sha2` and `hex` crates. `Mac::verify_slice` compares in
constant time.

```rust
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// `body` is the raw request body; `header` is the `X-TxWatch-Signature` value.
fn verify(body: &[u8], secret: &str, header: Option<&str>) -> bool {
    let Some(hex_sig) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(sig) = hex::decode(hex_sig) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}
```

## TypeScript (Node.js)

With Express, capture the raw body with `express.raw` rather than `express.json`.

```ts
import { createHmac, timingSafeEqual } from "node:crypto";
import express from "express";

const SECRET = process.env.TXWATCH_WEBHOOK_SECRET!;

function verify(body: Buffer, header: string | undefined): boolean {
  if (!header?.startsWith("sha256=")) return false;
  const received = Buffer.from(header.slice("sha256=".length), "hex");
  const expected = createHmac("sha256", SECRET).update(body).digest();
  // timingSafeEqual throws on a length mismatch, so check it first.
  return received.length === expected.length && timingSafeEqual(received, expected);
}

const app = express();
app.post("/txwatch", express.raw({ type: "application/json" }), (req, res) => {
  if (!verify(req.body, req.get("X-TxWatch-Signature"))) {
    return res.sendStatus(401);
  }
  const alert = JSON.parse(req.body.toString("utf8"));
  // ... handle alert ...
  res.sendStatus(204);
});
```

## Python

With Flask, read `request.get_data()` (the raw bytes), not `request.json`.

```python
import hashlib
import hmac
import os

from flask import Flask, abort, request

SECRET = os.environ["TXWATCH_WEBHOOK_SECRET"].encode()
app = Flask(__name__)


def verify(body: bytes, header: str | None) -> bool:
    if not header or not header.startswith("sha256="):
        return False
    expected = hmac.new(SECRET, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(header[len("sha256="):], expected)


@app.post("/txwatch")
def txwatch():
    body = request.get_data()
    if not verify(body, request.headers.get("X-TxWatch-Signature")):
        abort(401)
    alert = request.get_json()
    # ... handle alert ...
    return "", 204
```

## Go

`hmac.Equal` compares in constant time.

```go
package main

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"io"
	"net/http"
	"os"
	"strings"
)

var secret = []byte(os.Getenv("TXWATCH_WEBHOOK_SECRET"))

func verify(body []byte, header string) bool {
	hexSig, ok := strings.CutPrefix(header, "sha256=")
	if !ok {
		return false
	}
	received, err := hex.DecodeString(hexSig)
	if err != nil {
		return false
	}
	mac := hmac.New(sha256.New, secret)
	mac.Write(body)
	return hmac.Equal(received, mac.Sum(nil))
}

func handler(w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(io.LimitReader(r.Body, 1<<20))
	if err != nil || !verify(body, r.Header.Get("X-TxWatch-Signature")) {
		w.WriteHeader(http.StatusUnauthorized)
		return
	}
	// ... json.Unmarshal(body, &alert) and handle it ...
	w.WriteHeader(http.StatusNoContent)
}

func main() {
	http.HandleFunc("/txwatch", handler)
	http.ListenAndServe(":8080", nil)
}
```
