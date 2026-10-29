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

*(a preencher com o teste)*
