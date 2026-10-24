# Regras do worker
- Trabalhe apenas dentro de D:\Projetos\winshare. Nunca use git destrutivo (reset, checkout --, clean, push, merge). Faça no máximo UM commit ao final.
- Siga .tasks/SPEC.md. Faça apenas a tarefa do arquivo de tarefa. Código Rust idiomático, tracing para logs, erros com anyhow.
- Verifique com `cargo build` e `cargo test` (código de saída real). Corrija erros de compilação antes de terminar. Não invente versões de crates: use `cargo add`.
- Se uma API de crate for incerta, leia o código-fonte em ~/.cargo/registry em vez de adivinhar.
- Relatório final CURTO: hash do commit, saída de cargo build/test, e o que ficou pendente.
