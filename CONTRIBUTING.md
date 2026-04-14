# Contributing to VTT

Thank you for your interest in contributing to the VTT blockchain.

## Getting Started

1. Fork the repository
2. Clone your fork
3. Create a feature branch: `git checkout -b feature/my-feature`
4. Make your changes
5. Run tests: `cargo test --all`
6. Run formatting: `cargo fmt --all`
7. Run linting: `cargo clippy --all-targets --all-features`
8. Commit and push
9. Open a Pull Request

## Code Style

- Follow existing patterns in the codebase
- Run `cargo fmt` before committing
- All clippy warnings must be resolved
- Add tests for new functionality

## Pull Request Process

1. PRs must pass all CI checks (fmt, clippy, test, bridge tests)
2. PRs require review before merging
3. Keep PRs focused on a single change
4. Write clear commit messages

## Bridge Contracts (Solidity)

For changes to `bridge-evm/`:

```bash
cd bridge-evm
forge fmt
forge test -v
```

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0.
