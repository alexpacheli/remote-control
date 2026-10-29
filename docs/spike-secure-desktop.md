# Spike: secure desktop no Windows (F4, marco 0)

**Pergunta:** um serviço SYSTEM consegue capturar e controlar a tela de login, o UAC e o
`Ctrl+Alt+Del`? É o maior risco da F4 (seção 13 do ARCHITECTURE).

**O que o spike faz** (`apps/hext-desktop-spike`, fora do produto):

- `install` (como administrador) liga a política `SoftwareSASGeneration` e cria e inicia o serviço
  `HextSpike`.
- O serviço lança um *agent* como SYSTEM na sessão do console (`WTSGetActiveConsoleSessionId`, token
  do serviço com `TokenSessionId` trocado, `CreateProcessAsUser`). Ele relança o agent quando a sessão
  muda.
- O agent acompanha o desktop de entrada (`OpenInputDesktop`, `SetThreadDesktop`) e captura a tela
  com DXGI Desktop Duplication, recriando a captura a cada troca. A cada 3 s ele salva a última
  imagem de cada desktop (`Default.bmp`, `Winlogon.bmp`).
- A cada 5 s, o agent move o mouse 1 pixel com `SendInput` e registra se entrou.
- `sas` pede ao serviço um `Ctrl+Alt+Del` (`SendSAS`).
- Tudo vai para `C:\ProgramData\HextSpike\log.txt`.
- `uninstall` remove o serviço e a política.

## Como testar

1. Baixe o `hext-desktop-spike.exe` do artefato Windows do CI desta branch.
2. Abra um terminal **como administrador** e rode `hext-desktop-spike.exe install`. Siga os passos
   que ele mostra: bloquear e desbloquear, UAC, `sas` e logoff.
3. Mande o `log.txt` e as imagens `.bmp`.
4. Rode `hext-desktop-spike.exe uninstall`.

## Resultados

### Rodada 1 (29/10/2026, Windows 11, 1920×1080)

- **O serviço lança o agent como SYSTEM na sessão do console:** ✅
- **Captura da área de trabalho (`Default`):** ✅
- **O agent segue a troca de desktop:** ✅ Ele trocou Default ↔ Winlogon 6 vezes. Na troca, o DXGI devolve
  `0x887A0026` (`DXGI_ERROR_ACCESS_LOST`, que o Windows traduz errado como "mutex abandonado"); o agent
  reabre a captura e continua.
- **Captura do Winlogon:** ✅ A imagem mostra a tela do `Ctrl+Alt+Del` (Bloquear, Trocar usuário, Sair,
  Gerenciador de Tarefas).
- **`Ctrl+Alt+Del` remoto (`SendSAS` do serviço):** ✅ Duas vezes, e as duas abriram a tela.
- **Input no Winlogon:** inconclusivo. O teste movia o cursor 1 pixel de forma relativa, e a aceleração
  do ponteiro às vezes come esse movimento, até na área de trabalho. Na rodada 2, o teste passou a
  mover o cursor para um ponto absoluto e conferir se ele chegou lá.
- **Ainda não visto:** o prompt do UAC e a tela de bloqueio em imagem, porque só a última imagem de cada
  desktop era salva. Também falta o logoff. A rodada 2 salva a primeira imagem de cada troca.

### Rodada 2 (29/10/2026, mesma máquina)

O teste do mouse agora leva o cursor a um ponto exato e confere se ele chegou.

- **Input no Winlogon:** ✅ Todas as 8 tentativas no Winlogon levaram o cursor ao ponto pedido: na tela
  de desbloqueio, no prompt do UAC e na tela do `Ctrl+Alt+Del`. Os "NÃO" do log são todos na área de
  trabalho, depois das 14:26:39, com o cursor a poucos pixels do ponto pedido: era a mão do usuário
  mexendo no mouse ao mesmo tempo.
- **Linha do tempo, pelo log:**
  - 14:24:57: Win+L. O Winlogon aparece por 1 s, e a primeira imagem é preta (a transição).
  - 14:25:12 a 14:25:17: a tela de bloqueio. No Windows 11, ela é desenhada no desktop `Default`, e a
    captura continua; a tela está parada, então são 0 quadros novos.
  - 14:25:20 a 14:25:23: o desbloqueio, com a senha no Winlogon (39 quadros). Captura e input ok.
  - 14:25:39 a 14:25:44: o prompt do UAC, no Winlogon. Captura e input ok.
  - 14:26:02: `SendSAS`. A tela do `Ctrl+Alt+Del` fica no Winlogon por 21 s, com captura e input ok.
- **Não testado:** o logoff (a sessão continuou a 1). Fica para o marco 1, em que o agent é relançado
  a cada troca de sessão.

- **As imagens confirmam:**
  - `03-Default`: a tela de bloqueio, com o relógio;
  - `04-Winlogon`: o fundo do desbloqueio;
  - `06-Winlogon`: o fundo escurecido do UAC, no primeiro quadro, antes do diálogo;
  - `08-Winlogon`: "Preparando as opções de segurança", no começo do `Ctrl+Alt+Del`;
  - `Winlogon.bmp`: o menu do `Ctrl+Alt+Del`.

## Conclusão

**O caminho da seção 3.4 do ARCHITECTURE funciona:**
- serviço SYSTEM;
- agent SYSTEM na sessão do console, lançado com o token do serviço;
- `OpenInputDesktop` com `SetThreadDesktop` a cada troca;
- DXGI recriada no `ACCESS_LOST`;
- `SendInput` no desktop atual;
- `SendSAS` do serviço com a política `SoftwareSASGeneration`.

Para o produto (marcos 1 e 2):
- Na troca de desktop, há um quadro preto de transição e o `ACCESS_LOST`. O agent precisa reabrir a
  captura sem derrubar a sessão e mandar um keyframe.
- A captura do produto (`hext-capture`) já recria a duplicação no `ACCESS_LOST`. Falta o
  `SetThreadDesktop` antes, que fica no agent.
- O `SendInput` do produto precisa rodar numa thread presa ao desktop de entrada atual.
