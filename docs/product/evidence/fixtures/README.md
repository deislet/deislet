# Measured fixtures

The exact source trees the compiler-identity evidence in `docs/product/evidence/`
was measured from, kept byte for byte so the digests those records carry can
still be checked against the repository.

`portable-app/` is the portable demo application as it stood when the
cross-platform identities were collected. Its project file uses the
`environment.variables` spelling, which the build has since stopped reading;
the live demo application is `scripts/demo-assets/portable-app/`. Nothing
builds these copies: they exist to be hashed.
