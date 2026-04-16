export ZSH="$HOME/.oh-my-zsh"

ZSH_THEME="robbyrussell"

plugins=(git docker kubectl)

source $ZSH/oh-my-zsh.sh

export PATH="$HOME/.local/bin:$PATH"

if command -v starship &> /dev/null; then
    eval "$(starship init zsh)"
fi

if [ -f ~/.zshrc.local ]; then
    source ~/.zshrc.local
fi