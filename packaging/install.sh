#!/bin/sh
# Установка wayswitch-rs: setgid-бинарник вместо доступа к клавиатуре для всех.
set -eu

cd "$(dirname "$0")/.."
BIN=/usr/local/bin/wayswitch-rs

if ! getent group input >/dev/null; then
    echo "В системе нет группы input — установка в этом режиме невозможна." >&2
    exit 1
fi

echo "1/4 Сборка (cargo build --release)"
cargo build --release --locked

echo "2/4 Установка $BIN: владелец root, группа input, setgid (нужен sudo)"
sudo install -o root -g input -m 2755 target/release/wayswitch-rs "$BIN"
sudo install -o root -g root -m 0644 packaging/60-wayswitch-rs.rules /etc/udev/rules.d/60-wayswitch-rs.rules
sudo install -d /usr/local/share/doc/wayswitch-rs
sudo install -m 0644 LICENSE LICENSE.upstream /usr/local/share/doc/wayswitch-rs/
sudo install -m 0644 data/LICENSE /usr/local/share/doc/wayswitch-rs/LICENSE.dictionaries
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput

echo "3/4 Служба systemd --user"
mkdir -p "$HOME/.config/systemd/user"
install -m 0644 packaging/wayswitch-rs.service "$HOME/.config/systemd/user/"
systemctl --user daemon-reload
systemctl --user enable wayswitch-rs.service
# restart, а не enable --now: при обновлении иначе осталась бы работать старая версия.
systemctl --user restart wayswitch-rs.service

case "${XDG_CURRENT_DESKTOP:-}" in
*GNOME*)
    echo "4/4 Расширение GNOME Shell (нужно для переключения раскладки)"
    dest="$HOME/.local/share/gnome-shell/extensions/wayswitch@siberia.ru"
    mkdir -p "$dest"
    cp packaging/gnome-extension/wayswitch@siberia.ru/* "$dest/"
    gnome-extensions enable wayswitch@siberia.ru 2>/dev/null ||
        echo "   Выйдите из сеанса и войдите снова, затем: gnome-extensions enable wayswitch@siberia.ru"
    ;;
*) echo "4/4 Расширение GNOME не нужно" ;;
esac

if id -nG | tr ' ' '\n' | grep -qx input; then
    cat <<'EOF'

ВНИМАНИЕ: вы состоите в группе input. Это значит, что ЛЮБАЯ ваша программа
может читать клавиатуру (и пароли). wayswitch-rs в этой группе не нуждается:
  sudo gpasswd -d "$USER" input   # затем перелогиньтесь
EOF
fi
echo
echo "Готово. Проверка: wayswitch-rs doctor"
