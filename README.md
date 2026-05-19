# 🎬 Cinnamon Wallpaper

**Aplicación nativa de fondos de pantalla animados de alto rendimiento específicamente diseñada para el entorno de escritorio Linux Mint Cinnamon.**

## ✨ Características

- 🚀 **Alto rendimiento**: Optimizado para GPU con aceleración por hardware
- 🎯 **Cinnamon nativo**: Integración perfecta con el escritorio Cinnamon
- 🔧 **Bajo consumo**: Uso mínimo de recursos del sistema
- 🖼️ **Iconos del escritorio**: Mantiene los iconos del escritorio visibles y funcionales
- 🎨 **Formatos múltiples**: Soporte para MP4, WebM, AVI, y más formatos de video

## 📋 Requisitos del Sistema

- **OS**: Linux Mint 20+ con entorno Cinnamon
- **GPU**: Tarjeta gráfica con soporte OpenGL 2.0+
- **RAM**: 512MB disponibles (recomendado: 1GB+)
- **Dependencias**: GStreamer 1.20+ (se instala automáticamente)

## Instalación

### Método 1: Instalar desde paquete .deb (Recomendado)

```bash
# Si tienes el archivo .deb local
sudo dpkg -i cinnamon-wallpaper_1.0.0-1_amd64.deb

# Instalar dependencias faltantes (si las hay)
sudo apt-get install -f
```

### Método 2: Compilar desde código fuente

```bash
# Instalar dependencias del sistema
sudo apt update
sudo apt install -y \
    libgtk-4-dev \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libgdk-pixbuf-2.0-dev \
    libx11-dev \
    libxrandr-dev \
    pkg-config \
    build-essential

# Instalar Rust (si no está instalado)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Compilar
cargo build --release

# Generar paquete .deb
cargo install cargo-deb
cargo deb
```

## Uso

### Uso básico

```bash
# Lanzar la interfaz gráfica (también se abre sin argumentos)
cinnamon-wallpaper --gui

# Establecer un video como fondo de pantalla en el monitor primario
cinnamon-wallpaper /ruta/al/video.mp4

# Aplicar a un monitor específico (índice 0-based)
cinnamon-wallpaper --monitor 1 /ruta/al/video.mp4

# Usar con un archivo de configuración personalizado
cinnamon-wallpaper --config ~/.config/cinnamon-wallpaper/config.toml /ruta/al/video.mp4

# Ejecutar en modo demonio (segundo plano)
cinnamon-wallpaper -D /ruta/al/video.mp4

# Restaurar el último wallpaper aplicado (usado por el autostart)
cinnamon-wallpaper --restore-last
```

### Opciones de línea de comandos

```bash
cinnamon-wallpaper [OPCIONES] [VIDEO_FILE]

OPCIONES:
  -c, --config <CONFIG>   Archivo de configuración personalizado
  -m, --monitor <INDEX>   Monitor específico (índice 0-based)
  -g, --gui               Lanzar la interfaz gráfica
  -D, --daemon            Ejecutar en segundo plano (modo demonio)
      --restore-last      Restaurar el último wallpaper aplicado (autostart)
      --list-monitors     Listar los monitores disponibles
      --desktop-info      Mostrar información del entorno de escritorio
  -d, --debug             Habilitar logging detallado (debug)
  -h, --help              Mostrar ayuda
  -V, --version           Mostrar versión
```

> El control de FPS y el audio se ajustan desde el archivo de configuración
> (`target_fps`, `audio_enabled`); no existen flags de CLI equivalentes.

## ⚙️ Configuración

El archivo de configuración se encuentra en: `~/.config/cinnamon-wallpaper/config.toml`

```toml
[video]
# Ruta al archivo de video
file = "/home/usuario/Videos/mi_wallpaper.mp4"

# Limitar FPS para ahorrar batería (0 = sin límite)
target_fps = 30

# Habilitar aceleración por hardware
hardware_acceleration = true

# Calidad de escalado para videos de menor resolución (720p/480p)
# 1-3: Escalado rápido sin coste adicional de CPU (recomendado para 720p/480p)
# 4-7: Calidad media con coste moderado  
# 8-10: Mejor calidad con mayor uso de CPU
scaling_quality = 2

[display]
# Monitor específico (-1 = todos los monitores)
monitor = -1

# Escalar el video para ajustar a la pantalla
scale_to_fit = true

[system]
# Nivel de logging (error, warn, info, debug)
log_level = "info"

# Directorio para archivos temporales
temp_dir = "/tmp/cinnamon-wallpaper"
```

## Inicio automático

### Método 1: Aplicaciones de inicio de Cinnamon

1. Abre **Aplicaciones de inicio** desde el menú de Cinnamon
2. Haz clic en **Agregar**
3. Completa los campos:
   - **Nombre**: Cinnamon Wallpaper
   - **Comando**: `cinnamon-wallpaper --daemon /ruta/a/tu/video.mp4`
   - **Comentario**: Fondo de pantalla animado
4. Haz clic en **Agregar**

### Método 2: Systemd (usuario)

```bash
# Crear servicio de usuario
mkdir -p ~/.config/systemd/user

cat > ~/.config/systemd/user/cinnamon-wallpaper.service << EOF
[Unit]
Description=Cinnamon Animated Wallpaper
After=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/bin/cinnamon-wallpaper --daemon /home/usuario/Videos/mi_video.mp4
Restart=on-failure
Environment=DISPLAY=:0

[Install]
WantedBy=default.target
EOF

# Habilitar e iniciar el servicio
systemctl --user daemon-reload
systemctl --user enable cinnamon-wallpaper.service
systemctl --user start cinnamon-wallpaper.service
```

## 🐛 Solución de problemas

### El video no se reproduce

```bash
# Verificar que GStreamer puede reproducir el video
gst-play-1.0 /ruta/al/video.mp4

# Instalar codecs adicionales si es necesario
sudo apt install ubuntu-restricted-extras
```

### Los iconos del escritorio desaparecen

```bash
# Forzar reinicio de Nemo (manejador de iconos)
nemo-desktop --quit
nemo-desktop &
```

### Rendimiento bajo

1. Reducir FPS en la configuración: `target_fps = 20`
2. Usar un video de menor resolución
3. Verificar aceleración por hardware: `hardware_acceleration = true`

### Verificar logs

```bash
# Ejecutar con logs detallados
RUST_LOG=debug ./target/release/cinnamon-wallpaper /ruta/al/video.mp4
```

## 🔧 Desarrollo

### Estructura del proyecto

```
cinnamon-wallpaper/
├── src/
│   ├── main.rs                # Punto de entrada y bucle principal
│   ├── config.rs              # Gestión de configuración (TOML)
│   ├── video.rs               # Procesamiento de video con GStreamer
│   ├── window.rs              # Gestión de ventanas X11
│   ├── desktop.rs             # Detección e integración con Cinnamon
│   ├── gsettings_wallpaper.rs # Manejo del wallpaper nativo de Cinnamon
│   ├── gui.rs                 # Interfaz gráfica (GTK4)
│   └── instance.rs            # Bloqueo de instancia única vía socket UNIX
├── assets/
│   ├── config.toml                          # Configuración por defecto
│   ├── cinnamon-wallpaper.desktop           # Lanzador de la aplicación
│   ├── cinnamon-wallpaper-autostart.desktop # Entrada de autostart
│   └── icons/LiveWallpaperIcon.svg          # Icono escalable
├── debian/                  # Scripts de mantenedor (.deb)
├── config.example.toml      # Ejemplo de configuración con todas las opciones
├── Dockerfile               # Imagen para compilar el paquete .deb
└── Cargo.toml               # Manifiesto Rust + metadatos cargo-deb
```

### Comandos de desarrollo

```bash
# Compilar en modo debug
cargo build

# Ejecutar con logs detallados
RUST_LOG=debug cargo run -- /ruta/al/video.mp4

# Ejecutar tests
cargo test

# Generar documentación
cargo doc --open

# Linting y formato
cargo clippy
cargo fmt

# Generar paquete .deb
cargo deb
```

## 📄 Licencia

Este proyecto está licenciado bajo la Licencia MIT - ver el archivo [LICENSE](LICENSE) para más detalles.

## 🤝 Contribuir

¡Las contribuciones son bienvenidas! Por favor:

1. Fork el proyecto
2. Crea una rama para tu feature (`git checkout -b feature/AmazingFeature`)
3. Commit tus cambios (`git commit -m 'Add some AmazingFeature'`)
4. Push a la rama (`git push origin feature/AmazingFeature`)
5. Abre un Pull Request

## Agradecimientos

- Equipo de desarrollo de Cinnamon por el excelente entorno de escritorio
- Proyecto GStreamer por las capacidades de procesamiento de video
- Comunidad de Rust por las herramientas y librerías increíbles
