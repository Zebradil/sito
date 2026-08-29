# Pass-through trust: sito holds no keys, verification stays in the Nix client

sito relays narinfo and NAR bytes unmodified. It never signs, re-signs, or
strips signatures, and holds no key material; the Nix client verifies narinfo
signatures against `trusted-public-keys` exactly as it would talking to the
upstream directly. Each upstream's config entry carries the public key(s) its
narinfos are signed with, and the nix modules aggregate those into
`trusted-public-keys`.

The only response sito authors itself is its own `/nix-cache-info`.

This keeps sito out of the trust chain entirely: a compromised or buggy sito
can deny service or pick a slow upstream, but cannot make the client accept an
unsigned or foreign path.
