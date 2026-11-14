# Arquitetura

[English](ARCHITECTURE.md) | **Português**

Como o Hext Remote funciona hoje (versão 0.8). Os porquês das principais escolhas estão em
[docs/DECISIONS.pt-BR.md](docs/DECISIONS.pt-BR.md), o que está pronto e o que vem a seguir em
[docs/STATUS.pt-BR.md](docs/STATUS.pt-BR.md), e o plano original, escrito antes do código, em
[docs/planos/plano-original.md](docs/planos/plano-original.md).

## 1. As peças

```
            ┌───────────────────────── hext.com.br (uma VM Linux) ─────────────────────────┐
            │  Caddy (TLS)  →  hext-server: IDs, presença, pedidos, contas (SQLite)        │
            │                  iroh-relay: só quando não há caminho direto                 │
            └──────────────────────────────────────────────────────────────────────────────┘
                    ▲ WebSocket (sinalização)                     ▲ relay (bytes cifrados)
                    │                                             │
  ┌─────────────────┴───┐      QUIC (iroh), de ponta a ponta     ┌┴────────────────────┐
  │ Técnico             │◄──────────────────────────────────────►│ Cliente             │
  │ ui + view           │  vídeo, input, área de transf., chat   │ ui + host / serviço │
  └─────────────────────┘                                        └─────────────────────┘
```

- **O app** (`apps/hext`): um executável só, `HextRemote.exe` / `Hext Remote.app` / `hext-remote`, com modos:
  - `ui`: a interface (Tauri 2; HTML, CSS e JavaScript simples em `apps/hext/ui`). Mostra o ID, os pedidos, a
    barra de sessão, a conta e as listas de computadores.
  - `view`: a janela da sessão, num processo nativo separado (`winit` + `wgpu`). O vídeo nunca passa por um
    webview, e um visualizador travado não derruba a interface. As janelas dividem a thread do event loop;
    cada sessão desenha numa thread própria, que o decodificador acorda direto, e nada que espere a GPU ou a
    tela roda onde as janelas se movem ([janela-travada](docs/estudos/janela-travada.md)).
  - `host`: compartilha este computador, dentro do processo da interface no modo portátil.
  - `service`: instala e roda o modo não assistido (seção 4).
- **O servidor** (`apps/hext-server`, `axum` + `tokio`): distribui os IDs de 9 dígitos, sabe quem está online,
  repassa os pedidos de conexão, guarda as contas (opcionais) e decide quem pode usar o relay. Nunca vê uma
  sessão. Protocolo e operação: [docs/hext-server.md](docs/hext-server.md).
- **O relay** ([`iroh-relay`](https://github.com/n0-computer/iroh)): repassa pacotes cifrados quando os dois
  computadores não se alcançam direto (NAT restrito, firewall corporativo, só HTTPS na porta 443).

## 2. Uma conexão

1. Na primeira execução, o app cria a **chave Ed25519** do computador (protegida pela DPAPI no Windows e pelo
   Keychain no macOS). Ele entra no servidor assinando um desafio e recebe o seu **ID**, aleatório e nunca
   sequencial.
2. O técnico pede um ID. O servidor repassa o pedido, com a chave do técnico, a esse computador.
3. Quem decide é o host (seção 5): pergunta a quem está na frente dele, ou confere a senha de acesso, a chave de
   um técnico vinculado ou a lista assinada de dispositivos da conta.
4. Os dois abrem uma conexão **QUIC** com o [`iroh`](https://iroh.computer): direta quando as redes permitem
   (hole punching), pelo relay senão. Cada lado prova a sua chave no handshake, e o visualizador fixa a chave do
   host (TOFU) e avisa se ela mudar.

## 3. Vídeo e canais

```
captura (GPU) → NV12 → codificação H.264 → pacotes + FEC → datagramas QUIC → decodificação → render wgpu
```

| Etapa | Windows | macOS | Reserva |
|---|---|---|---|
| Captura | DXGI Desktop Duplication (também a tela de login e o UAC, pelo serviço) | ScreenCaptureKit, no tamanho lógico da tela | — |
| Codificação | Media Foundation (placa de vídeo), sem cópia desde a captura | VideoToolbox | OpenH264 (processador) |
| Decodificação | Media Foundation com Direct3D 11 | VideoToolbox | OpenH264 |

- Só H.264, com configuração de baixa latência e sem B-frames. Keyframes sob demanda (início, perda, troca de
  monitor).
- O vídeo viaja em **datagramas QUIC**: um pacote perdido não trava nada. Cada quadro vira blocos de até 64
  pacotes com paridade Reed-Solomon (20%, 40% nos keyframes), intercalados e enviados com **pacing** (no máximo
  4× o bitrate), para um keyframe não sair numa rajada. O bitrate se adapta ao caminho.
- O **cursor** não vai no vídeo: o host manda a forma e a posição, e o visualizador desenha localmente.
- **Vários monitores:** o visualizador troca com `Ctrl+Alt+1..9` ou com os botões numerados da borda esquerda.
- **Monitor virtual:** sem monitor ligado, o host cria um para a sessão e o remove no fim (driver MttVDD no
  Windows, opcional no instalador; `CGVirtualDisplay` no macOS). `Ctrl+Alt+0` liga e troca a resolução.
  Detalhes: [docs/monitor-virtual.md](docs/monitor-virtual.md).
- **Som:** o que o host toca (WASAPI em loopback no Windows, ScreenCaptureKit no macOS 13+) vai em Opus, 48 kHz,
  10 ms por datagrama (cada um levando também o anterior), fora do pacing e do FEC do vídeo, e nada em silêncio; o
  visualizador guarda uns 40 ms numa fila que nunca passa de 70 ms e toca pelo `cpal`. `Ctrl+Alt+S` silencia.
  Detalhes: [docs/planos/F7.md](docs/planos/F7.md).

Os outros canais, em streams QUIC confiáveis: teclado por scancode (funciona entre layouts diferentes) e mouse
(`SendInput`, `CGEvent`), texto da área de transferência, arquivos (arrastar para a janela; chegam em Downloads),
chat, reinício remoto com reconexão sozinha e `Ctrl+Alt+Del` (`Ctrl+Alt+End`) pelo serviço do Windows.

## 4. Modo não assistido

**Windows** (o `.msi`, ou `service install`):

```
hext service (SYSTEM, sessão 0)  sinalização, presença, conferência do acesso, auditoria, atualização
   └─ agent (na sessão ativa, ou na tela de login / UAC)  captura, codificação, input
ui (sempre como o usuário)  aberta pelo serviço, fala com ele por um pipe que só os dois abrem
```

- Um serviço na sessão 0 não consegue capturar a tela. Por isso ele abre o agent na sessão ativa, e o abre de
  novo quando a tela muda para o login ou um prompt do UAC.
- A interface nunca roda como SYSTEM: um webview com direitos do sistema seria um caminho de escalada de
  privilégio.
- Instalado, o computador usa o **ID da máquina** (o mesmo para todos os usuários, mantido numa reinstalação).
  Configurações, logs e auditoria ficam em `C:\ProgramData\HextRemote`, que só administradores abrem.

**macOS:** o app abre no login (um LaunchAgent do usuário) e oferece o mesmo acesso não assistido (senha,
técnicos vinculados, conta), guardado na pasta do usuário. Depois de reiniciar, volta quando alguém entra na
conta: não há serviço do sistema para a tela de login ([decisões](docs/DECISIONS.pt-BR.md#mac-sem-tela-de-login)).

**Linux:** a interface, só como técnico: conta, Meus computadores, recentes e acessar outros computadores, cada
um na janela do visualizador. Ser acessado (host) vem depois.

## 5. Quem entra

Quem decide é o host, nunca o servidor. Um servidor ou um banco invadido não abre nenhum computador.

| Forma | Como |
|---|---|
| **Com aceite** (padrão) | Quem está no computador vê o nome e o ID do técnico e escolhe as permissões: controlar, área de transferência, arquivos, reiniciar. |
| **Senha de acesso** | Definida pelo dono. Conferida por **SPAKE2**: a senha nunca trafega, e nem o servidor nem o relay conseguem descobri-la. |
| **Técnico vinculado** | Um técnico aceito uma vez pode ser vinculado: o host guarda a chave dele e o deixa entrar sem perguntar. Revogável no host. |
| **Conta** | A conta é uma lista de dispositivos em que **cada entrada é assinada por um dispositivo que já está nela**. O host vinculado à conta guarda essa lista e confere as assinaturas, então o servidor não consegue forjar uma entrada. Compartilhar com outra conta e revogar também são entradas assinadas. |

**A transparência não é opcional:** toda sessão mostra no host uma barra com quem está conectado e um botão para
encerrar, e cada acesso vai para a auditoria do computador. Não existe modo oculto
([decisão](docs/DECISIONS.pt-BR.md)). Limites de tentativas no servidor e no host atrasam quem tenta adivinhar
IDs ou senhas.

Contas (opcionais): e-mail, senha e TOTP, com códigos de recuperação; gerenciadas só no app. O servidor as
guarda em SQLite, e uma conta não confirmada é apagada em 24 horas.

## 6. Distribuição e atualização

- O `.github/workflows/release.yml` gera tudo a partir de uma tag `v*` e abre uma release em **rascunho**; uma
  pessoa a publica. Cada instalador é testado no CI (instalar, rodar, desinstalar).
- Windows: `.msi` (app, serviço, driver opcional do monitor virtual) e `.exe` portátil, **sem assinatura de
  código**: o SmartScreen avisa na primeira abertura. macOS: `.dmg`, assinado com Developer ID e notarizado.
  Linux: `.deb` e `.rpm` (x86_64 e ARM64), `.AppImage` (x86_64).
- **Atualização:** cada release leva o `latest.json` (versões e SHA-256 de cada instalador), assinado com uma
  chave Ed25519 guardada como segredo do repositório, pelo `hext-release`, um programa pequeno que só faz isso (sem
  interface, sem rede); o app só aceita essa assinatura. O serviço do Windows
  confere a cada hora e instala entre uma sessão e outra; o app do Mac se troca em Aplicativos (só com
  Developer ID); nos outros casos, a tela inicial oferece o download.

## 7. Plataformas

| | Windows | macOS | Linux |
|---|---|---|---|
| Técnico (visualizador) | ✅ | ✅ | ✅ (interface) |
| Cliente (host) | ✅, inclusive tela de login e UAC | ✅, depois do login | só imagem de teste |
| Não assistido | ✅ serviço | ✅ depois do login | — |
| Monitor virtual | ✅ (driver opcional) | ✅ | — |

## 8. Código

As pastas estão no [README](README.pt-BR.md#estrutura). O código de cada sistema fica atrás de
`#[cfg(target_os = ...)]`, e o CI compila e testa no Windows, no macOS e no Linux a cada push. Os comentários no
código são em inglês; a interface e a maior parte dos documentos internos, em português.
