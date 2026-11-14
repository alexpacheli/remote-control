# Hext Remote

[English](README.md) | **Português**

Acesso e suporte remoto para Windows, macOS e Linux, focado em fluidez. Software livre ([AGPL-3.0](LICENSE)),
escrito em Rust.

- **Conexão pelo ID:** cada computador tem um ID de 9 dígitos. O técnico digita o ID, e quem está do outro lado
  aceita ou recusa.
- **Direto quando possível:** os dois computadores se conectam direto, mesmo em redes diferentes. Quando não dá
  (um firewall corporativo, por exemplo), a sessão passa por um relay, sempre cifrada de ponta a ponta.
- **Vídeo pela placa de vídeo:** captura, codificação H.264 e exibição pela GPU, com volta automática para o
  processador.
- **Acesso não assistido:** com o serviço instalado, o computador fica online desde o boot. O técnico entra com
  uma senha definida pelo dono ou, depois, sem senha, pela chave dele ou pela conta. No Windows, isso vale
  também na tela de login e no UAC.
- **Na sessão:** vários monitores, área de transferência, arquivos, chat, reinício remoto e até 8 sessões ao
  mesmo tempo.
- **Computador sem monitor:** um monitor virtual opcional (Windows e macOS) dá imagem à sessão num servidor ou
  num Mac mini sem tela ligada.
- **Contas (opcionais):** lista de computadores sincronizada, compartilhamento com outras contas e registro dos
  acessos.
- **Transparente:** toda sessão aparece na tela enquanto dura, e cada acesso fica registrado no computador.

## Como funciona

```
                    ┌──────────────────────────────┐
         ┌─────────►│  Servidor de sinalização     │◄─────────┐
         │          │  (hext-server): IDs,         │          │
         │          │  presença e pedidos          │          │
         │          └──────────────────────────────┘          │
         │                                                    │
  ┌──────┴───────┐     sessão direta (P2P, QUIC)       ┌──────┴───────┐
  │   Técnico    │◄───────────────────────────────────►│   Cliente    │
  │ Hext Remote  │                                     │ Hext Remote  │
  └──────┬───────┘                                     └──────┬───────┘
         │          ┌──────────────────────────────┐          │
         └─────────►│  Relay (iroh-relay): só sem  │◄─────────┘
                    │  conexão direta; não vê o    │
                    │  conteúdo                    │
                    └──────────────────────────────┘
```

1. Ao abrir, o app se registra no **servidor de sinalização** com a chave do computador (Ed25519, criada na
   primeira execução) e recebe o seu ID.
2. O técnico pede para conectar a um ID. O servidor repassa o pedido, e o outro lado aceita, ou confere a senha
   ou a chave do técnico.
3. Os dois abrem uma conexão **QUIC** entre si, atravessando o NAT. Se não conseguirem, usam o **relay**.
4. A sessão é cifrada de ponta a ponta entre os dois computadores: nem o servidor nem o relay veem a tela, o
   teclado ou os arquivos. A senha de acesso nunca trafega (SPAKE2).

Detalhes na [arquitetura](ARCHITECTURE.pt-BR.md). Por que é feito assim: [docs/DECISIONS.pt-BR.md](docs/DECISIONS.pt-BR.md);
em que pé está: [docs/STATUS.pt-BR.md](docs/STATUS.pt-BR.md); todos os documentos: [docs/](docs/README.pt-BR.md).

## Baixar e usar

Os instaladores ficam em [Releases](https://github.com/alexpacheli/hext-remote/releases):

| Sistema | Arquivo | Observação |
|---|---|---|
| Windows | `HextRemote-<versão>-x64.msi` | Instala o app e o serviço (acesso não assistido, tela de login). Para muitos computadores: `msiexec /i HextRemote-<versão>-x64.msi /qn`. |
| Windows | `HextRemote-<versão>-portable.exe` | Para um atendimento rápido, sem instalar. |
| macOS | `HextRemote-<versão>.dmg` | Arraste o Hext Remote para Aplicativos. |
| Linux | `hext-remote_<versão>-1_amd64.deb` (Ubuntu 22.04+, Debian) ou `hext-remote-<versão>-1.x86_64.rpm` (Fedora); em ARM, `_arm64.deb` e `.aarch64.rpm`; também `.AppImage` (x86_64) | `sudo apt install ./hext-remote_<versão>-1_amd64.deb` ou `sudo dnf install ./hext-remote-<versão>-1.x86_64.rpm` (trazem as bibliotecas de que precisam); depois, aparece no menu. Por enquanto, só para acessar outros computadores: não tem ID, então não pode ser acessado. |

Os instaladores do Windows não têm assinatura de código: na primeira abertura, o SmartScreen avisa (Mais
informações → Executar assim mesmo). O app do macOS é assinado com um Apple Developer ID e notarizado. Veja a
[privacidade](#privacidade).

**Primeiro atendimento:**

1. No computador do cliente, abra o Hext Remote. A tela inicial mostra o **ID** dele.
2. No computador do técnico, abra o Hext Remote, digite esse ID e clique em **Conectar**.
3. No cliente, aparece o pedido com o nome do técnico e as permissões. Ao aceitar, a sessão começa.

Para entrar sem ninguém aceitar, instale o serviço (o `.msi` já instala) e defina a senha de acesso na aba
**Segurança**. O roteiro completo de cada recurso, inclusive pela linha de comando, está em
[docs/roteiros/testes.md](docs/roteiros/testes.md), e os roteiros de atendimento em campo, em
[docs/roteiros/](docs/roteiros/).

**Atualização.** Instalado no Windows (o serviço) ou no Mac (em Aplicativos), o Hext Remote se atualiza
sozinho, entre uma sessão e outra. Nos outros casos, a tela inicial avisa quando há uma versão nova e abre a
página dela (no Linux, instale o pacote novo com `apt` ou `dnf`, como acima). Cada versão é assinada pelo pipeline de release, e o app recusa qualquer outra.

## Servidor próprio

A versão oficial usa o nosso servidor (`signal.hext.com.br` e `relay.hext.com.br`). Quem atende muitos
computadores, quer os dados na própria infraestrutura ou distribui uma versão modificada sobe o próprio servidor
e compila o app apontando para ele. O servidor e o relay são deste repositório e do projeto
[iroh](https://github.com/n0-computer/iroh), sem nenhum serviço pago.

Em resumo: um servidor Linux com IP público (1 a 2 vCPUs e 1 a 2 GB de RAM para começar), dois nomes no DNS
(`signal.<domínio>` e `relay.<domínio>`), as portas 80/443 TCP e 7842 UDP, depois `docker compose up` ou
`./deploy/install.sh`, e um app compilado a partir do seu fork com o endereço do seu servidor.

**Guia passo a passo: [docs/self-hosting.pt-BR.md](docs/self-hosting.pt-BR.md)**: servidor (Docker ou systemd),
e-mails da conta, app próprio, chave de atualização, backup, tráfego e segurança.

## Compilar

Precisa do [Rust](https://rustup.rs). A versão é fixada em `rust-toolchain.toml` e instalada sozinha. No Windows,
precisa também do Visual Studio Build Tools com a carga "Desenvolvimento para desktop com C++". O codec do som
(libopus) é compilado pelo [CMake](https://cmake.org): no PATH (no Windows, `winget install Kitware.CMake`; o que vem com
o Build Tools também serve, pela variável `CMAKE`). No Linux, também `libasound2-dev` (ALSA).

```sh
cargo run -p hext              # a interface
cargo run -p hext -- --help    # a linha de comando
cargo run -p hext-server       # o servidor, em http://127.0.0.1:8080 (teste: --server ws://127.0.0.1:8080)
cargo test --workspace         # testes (inclui um de ponta a ponta host → cliente)
```

Variáveis da compilação, todas opcionais:

| Variável | O que define | Sem ela |
|---|---|---|
| `HEXT_SERVER` | Servidor de sinalização | `wss://signal.hext.com.br` |
| `HEXT_RELAY` | Relay | `https://relay.hext.com.br`; com outro `HEXT_SERVER`, nenhum (só conexão direta) |
| `HEXT_RELEASES` | Página das versões, onde o app procura atualizações | as Releases deste repositório |
| `HEXT_UPDATE_KEY` | Chave pública que assina as atualizações | sem atualização automática |

## Estrutura

| Pasta | Conteúdo |
|---|---|
| `apps/hext` | O app: interface (Tauri), `host`, `view` e `service` |
| `apps/hext-server` | Servidor: IDs, presença, sinalização e contas ([docs/hext-server.md](docs/hext-server.md)) |
| `crates/proto` | Mensagens do protocolo: handshake, vídeo, input |
| `crates/crypto` | Identidade do computador (Ed25519) e senha de acesso (SPAKE2) |
| `crates/signal` | Sinalização: ID de 9 dígitos, mensagens entre o app e o servidor, cliente WebSocket |
| `crates/net` | Transporte (`iroh`/QUIC): direto quando possível, relay senão |
| `crates/capture` | Captura de tela (DXGI no Windows, ScreenCaptureKit no macOS) e o monitor virtual |
| `crates/codec` | H.264: placa de vídeo (Media Foundation, VideoToolbox) ou OpenH264 em software |
| `crates/input` | Mapa de teclas e injeção de mouse e teclado (SendInput, CGEvent) |
| `crates/render` | Renderização na GPU (wgpu), YUV → RGB no shader |
| `crates/audio` | Som: captura do que o computador toca (WASAPI, ScreenCaptureKit), Opus, saída (cpal) |
| `crates/session` | Pipelines do host e do cliente |
| `crates/metrics` | Estatísticas de latência |
| `deploy` | Servidor: script do systemd, Caddy, relay e Docker |
| `installer` | Instaladores (WiX no Windows, `.dmg` no macOS, pacotes do Linux) |

## Contribuir

Sugestões e relatos de problemas são bem-vindos em [Issues](https://github.com/alexpacheli/hext-remote/issues).
Para enviar uma mudança, abra um pull request. O CI roda nos três sistemas:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked
cargo test --workspace --locked
```

Ao contribuir, você concorda em licenciar a sua contribuição sob a mesma licença do projeto.

## Licença

O Hext Remote é software livre, sob a [GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`).
Quem distribuir o programa modificado, ou oferecer uma versão modificada do servidor pela rede, precisa
publicar o código-fonte dessa versão sob a mesma licença.

## Privacidade

O programa envia dados só para onde os seus recursos precisam:

- ao servidor de sinalização do Hext: o ID, a chave pública, o nome e a versão do dispositivo, para encontrar o
  outro computador;
- com uma conta (opcional): o e-mail e os dados de entrada, a lista assinada de dispositivos e computadores da
  conta e, para um computador vinculado à conta, o registro de acessos (quem conectou e quando), mostrado na
  tela Atividade da conta. Os códigos de verificação vão para esse e-mail por um provedor de e-mail;
- ao relay: a própria sessão, cifrada de ponta a ponta, quando não há conexão direta (o relay não vê o
  conteúdo);
- ao GitHub: a busca e o download de atualizações.

A tela, o teclado, os arquivos e a área de transferência de uma sessão vão só para o outro computador, cifrados
de ponta a ponta. Nada é vendido ou compartilhado com terceiros, e não há publicidade nem estatísticas de uso.
Contas não confirmadas por e-mail são excluídas em até 24 horas, e uma conta pode ser excluída a qualquer
momento pelo app (Conta → Excluir a conta).

Ninguém conecta a um computador sem o aceite na tela, sem a senha de acesso definida pelo dono ou sem um
dispositivo de uma conta que o dono vinculou. Toda sessão aparece na tela enquanto dura, e o app instalado
guarda um registro local delas.
