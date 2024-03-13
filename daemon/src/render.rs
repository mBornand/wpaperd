use std::{
    cell::RefCell,
    ffi::{c_void, CStr},
    ops::Deref,
    rc::Rc,
};

use color_eyre::{
    eyre::{bail, ensure, Context},
    Result,
};
use egl::API as egl;
use image::{DynamicImage, RgbaImage};
use smithay_client_toolkit::reexports::client::{protocol::wl_surface::WlSurface, Proxy};
use wayland_egl::WlEglSurface;

use crate::{surface::DisplayInfo, wallpaper_info::BackgroundMode};

pub mod gl {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));

    pub use Gles2 as Gl;
}

fn transparent_image() -> RgbaImage {
    RgbaImage::from_raw(1, 1, vec![0, 0, 0, 0]).unwrap()
}

// Macro that check the error code of the last OpenGL call and returns a Result.
macro_rules! gl_check {
    ($gl:expr, $desc:tt) => {{
        let error = $gl.GetError();
        if error != gl::NO_ERROR {
            let error_string = $gl.GetString(error);
            ensure!(
                !error_string.is_null(),
                "OpenGL error when {}: {}",
                $desc,
                error
            );

            let error_string = CStr::from_ptr(error_string as _)
                .to_string_lossy()
                .into_owned();
            bail!("OpenGL error when {}: {} ({})", $desc, error, error_string);
        }
    }};
}

fn load_texture(gl: &gl::Gl, image: DynamicImage) -> Result<gl::types::GLuint> {
    Ok(unsafe {
        let mut texture = 0;
        gl.GenTextures(1, &mut texture);
        gl_check!(gl, "generating textures");
        gl.ActiveTexture(gl::TEXTURE0);
        gl_check!(gl, "activating textures");
        gl.BindTexture(gl::TEXTURE_2D, texture);
        gl_check!(gl, "binding textures");
        gl.TexImage2D(
            gl::TEXTURE_2D,
            0,
            gl::RGBA8.try_into().unwrap(),
            image.width().try_into().unwrap(),
            image.height().try_into().unwrap(),
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            image.as_bytes().as_ptr() as *const c_void,
        );
        gl_check!(gl, "defining the texture");
        gl.GenerateMipmap(gl::TEXTURE_2D);
        gl_check!(gl, "generating the mipmap");
        gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl_check!(gl, "defining the texture min filter");
        gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl_check!(gl, "defining the texture mag filter");

        texture
    })
}

pub struct Renderer {
    gl: gl::Gl,
    pub program: gl::types::GLuint,
    vao: gl::types::GLuint,
    vbo: gl::types::GLuint,
    eab: gl::types::GLuint,
    // milliseconds time for the animation
    animation_time: u32,
    pub time_started: u32,
    display_info: Rc<RefCell<DisplayInfo>>,
    old_wallpaper: Wallpaper,
    current_wallpaper: Wallpaper,
    transparent_texture: gl::types::GLuint,
    animation_fit_changed: bool,
}

pub struct Wallpaper {
    texture: gl::types::GLuint,
    image_width: u32,
    image_height: u32,
    display_info: Rc<RefCell<DisplayInfo>>, // transparent_texture: gl::types::GLuint,
}

struct Coordinates {
    x_left: f32,
    x_right: f32,
    y_bottom: f32,
    y_top: f32,
}

impl Coordinates {
    const VEC_X_LEFT: f32 = -1.0;
    const VEC_X_RIGHT: f32 = 1.0;
    const VEC_Y_BOTTOM: f32 = 1.0;
    const VEC_Y_TOP: f32 = -1.0;

    const TEX_X_LEFT: f32 = 0.0;
    const TEX_X_RIGHT: f32 = 1.0;
    const TEX_Y_BOTTOM: f32 = 0.0;
    const TEX_Y_TOP: f32 = 1.0;

    const fn new(x_left: f32, x_right: f32, y_bottom: f32, y_top: f32) -> Self {
        Self {
            x_left,
            x_right,
            y_bottom,
            y_top,
        }
    }

    const fn default_vec_coordinates() -> Self {
        Self {
            x_right: Self::VEC_X_RIGHT,
            x_left: Self::VEC_X_LEFT,
            y_bottom: Self::VEC_Y_BOTTOM,
            y_top: Self::VEC_Y_TOP,
        }
    }

    const fn default_texture_coordinates() -> Self {
        Self {
            x_right: Self::TEX_X_RIGHT,
            x_left: Self::TEX_X_LEFT,
            y_bottom: Self::TEX_Y_BOTTOM,
            y_top: Self::TEX_Y_TOP,
        }
    }
}

impl Wallpaper {
    pub const fn new(display_info: Rc<RefCell<DisplayInfo>>) -> Self {
        Self {
            texture: 0,
            image_width: 10,
            image_height: 10,
            display_info,
        }
    }

    pub fn bind(&self, gl: &gl::Gl) -> Result<()> {
        unsafe {
            gl.BindTexture(gl::TEXTURE_2D, self.texture);
            gl_check!(gl, "binding textures");
        }

        Ok(())
    }

    pub fn load_image(&mut self, gl: &gl::Gl, image: DynamicImage) -> Result<()> {
        self.image_width = image.width();
        self.image_height = image.height();

        let texture = load_texture(gl, image)?;

        unsafe {
            // Delete from memory the previous texture
            gl.DeleteTextures(1, &self.texture);
        }
        self.texture = texture;

        Ok(())
    }

    fn generate_texture_coordinates(&self, mode: BackgroundMode) -> Coordinates {
        // adjusted_width and adjusted_height returns the rotated sizes in case
        // the display is rotated. However, openGL is drawing in the same orientation
        // as our display (i.e. we don't apply any transform here)
        // We still need the scale
        let display_width = self.display_info.borrow().scaled_width();
        let display_height = self.display_info.borrow().scaled_height();
        let display_ratio = display_width as f32 / display_height as f32;
        let image_ratio = self.image_width as f32 / self.image_height as f32;

        match mode {
            BackgroundMode::Stretch => Coordinates::default_texture_coordinates(),
            BackgroundMode::Fit => Coordinates::default_texture_coordinates(),
            BackgroundMode::Fill if display_ratio == image_ratio => {
                Coordinates::default_texture_coordinates()
            }
            BackgroundMode::Fill if display_ratio > image_ratio => {
                // Same as width calculation below , but with inverted parameters
                // This is the expanded expression
                // adjusted_height = image_width as f32 / display_ratio;
                // y = (1.0 - image_height as f32 / adjusted_height) / 2.0;
                // We can simplify by just doing display_ration / image_ratio
                let y = (1.0 - display_ratio / image_ratio) / 2.0;
                Coordinates::new(
                    Coordinates::TEX_X_LEFT,
                    Coordinates::TEX_X_RIGHT,
                    Coordinates::TEX_Y_BOTTOM - y,
                    Coordinates::TEX_Y_TOP + y,
                )
            }
            BackgroundMode::Fill => {
                // Calculte the adjusted width, i.e. the width that the image should have to
                // have the same ratio as the display
                // adjusted_width = image_height as f32 * display_ratio;
                // Calculate the ratio between the adjusted_width and the image_width
                // x = (1.0 - adjusted_width / image_width as f32) / 2.0;
                // Simplify the expression and do the same as above
                let x = (1.0 - display_ratio / image_ratio) / 2.0;
                Coordinates::new(
                    Coordinates::TEX_X_LEFT + x,
                    Coordinates::TEX_X_RIGHT - x,
                    Coordinates::TEX_Y_BOTTOM,
                    Coordinates::TEX_Y_TOP,
                )
            }
            BackgroundMode::Tile => {
                // Tile using the original image size
                let x = display_width as f32 / self.image_width as f32;
                let y = display_height as f32 / self.image_height as f32;
                Coordinates::new(Coordinates::TEX_X_LEFT, x, Coordinates::TEX_Y_BOTTOM, y)
            }
        }
    }

    fn generate_vertices_coordinates_for_fit_mode(&self) -> Coordinates {
        let display_width = self.display_info.borrow().scaled_width();
        let display_height = self.display_info.borrow().scaled_height();
        let display_ratio = display_width as f32 / display_height as f32;
        let image_ratio = self.image_width as f32 / self.image_height as f32;
        if display_ratio == image_ratio {
            Coordinates::default_vec_coordinates()
        } else if display_ratio > image_ratio {
            let x = image_ratio / display_ratio;
            Coordinates::new(-x, x, Coordinates::VEC_Y_BOTTOM, Coordinates::VEC_Y_TOP)
        } else {
            let y = 1.0 - display_ratio / image_ratio;
            Coordinates::new(
                Coordinates::VEC_X_LEFT,
                Coordinates::VEC_X_RIGHT,
                Coordinates::VEC_Y_BOTTOM - y,
                Coordinates::VEC_Y_TOP + y,
            )
        }
    }
}

pub struct EglContext {
    pub display: egl::Display,
    pub context: egl::Context,
    pub config: egl::Config,
    wl_egl_surface: WlEglSurface,
    surface: khronos_egl::Surface,
    // pub surface: egl::Surface,
    // pub wl_egl_surface: WlEglSurface,
}

impl EglContext {
    pub fn new(egl_display: egl::Display, wl_surface: &WlSurface) -> Self {
        const ATTRIBUTES: [i32; 7] = [
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::NONE,
        ];

        let config = egl
            .choose_first_config(egl_display, &ATTRIBUTES)
            .expect("unable to choose an EGL configuration")
            .expect("no EGL configuration found");

        const CONTEXT_ATTRIBUTES: [i32; 5] = [
            egl::CONTEXT_MAJOR_VERSION,
            3,
            egl::CONTEXT_MINOR_VERSION,
            2,
            egl::NONE,
        ];

        let context = egl
            .create_context(egl_display, config, None, &CONTEXT_ATTRIBUTES)
            .expect("unable to create an EGL context");

        // First, create a small surface, we don't know the size of the output yet
        let wl_egl_surface = WlEglSurface::new(wl_surface.id(), 10, 10).unwrap();

        let surface = unsafe {
            egl.create_window_surface(
                egl_display,
                config,
                wl_egl_surface.ptr() as egl::NativeWindowType,
                None,
            )
            .expect("unable to create an EGL surface")
        };

        Self {
            display: egl_display,
            context,
            config,
            surface,
            wl_egl_surface,
        }
    }

    pub fn make_current(&self) -> Result<()> {
        egl.make_current(
            self.display,
            Some(self.surface),
            Some(self.surface),
            Some(self.context),
        )
        .with_context(|| "unable to make the context current")
    }

    // Swap the buffers of the surface
    pub fn swap_buffers(&self) -> Result<()> {
        egl.swap_buffers(self.display, self.surface)
            .with_context(|| "unable to post the surface content")
    }

    /// Resize the surface
    /// Resizing the surface means to destroy the previous one and then recreate it
    pub fn resize(&mut self, wl_surface: &WlSurface, width: i32, height: i32) {
        egl.destroy_surface(self.display, self.surface).unwrap();
        let wl_egl_surface = WlEglSurface::new(wl_surface.id(), width, height).unwrap();

        let surface = unsafe {
            egl.create_window_surface(
                self.display,
                self.config,
                wl_egl_surface.ptr() as egl::NativeWindowType,
                None,
            )
            .expect("unable to create an EGL surface")
        };

        self.surface = surface;
        self.wl_egl_surface = wl_egl_surface;
    }
}

impl Renderer {
    pub unsafe fn new(image: DynamicImage, display_info: Rc<RefCell<DisplayInfo>>) -> Result<Self> {
        let gl = gl::Gl::load_with(|name| {
            egl.get_proc_address(name).unwrap() as *const std::ffi::c_void
        });
        let vertex_shader = create_shader(&gl, gl::VERTEX_SHADER, VERTEX_SHADER_SOURCE)
            .expect("vertex shader creation succeed");
        let fragment_shader = create_shader(&gl, gl::FRAGMENT_SHADER, FRAGMENT_SHADER_SOURCE)
            .expect("fragment_shader");

        let program = gl.CreateProgram();
        gl_check!(gl, "calling CreateProgram");
        gl.AttachShader(program, vertex_shader);
        gl_check!(gl, "attach vertex shader");
        gl.AttachShader(program, fragment_shader);
        gl_check!(gl, "attach fragment shader");
        gl.LinkProgram(program);
        gl_check!(gl, "linking the program");
        gl.UseProgram(program);
        {
            // This shouldn't be needed, gl_check already checks the status of LinkProgram
            let mut status: i32 = 0;
            gl.GetProgramiv(program, gl::LINK_STATUS, &mut status as *mut _);
            ensure!(status == 1, "Program was not linked correctly");
        }
        gl_check!(gl, "calling UseProgram");
        gl.DeleteShader(vertex_shader);
        gl_check!(gl, "deleting the vertex shader");
        gl.DeleteShader(fragment_shader);
        gl_check!(gl, "deleting the fragment shader");
        gl.UseProgram(program);
        gl_check!(gl, "calling UseProgram");

        let (vao, vbo, eab) = initialize_objects(&gl)?;

        gl.Uniform1i(0, 0);
        gl_check!(gl, "calling Uniform1i");
        gl.Uniform1i(1, 1);
        gl_check!(gl, "calling Uniform1i");

        let old_wallpaper = Wallpaper::new(display_info.clone());
        let current_wallpaper = Wallpaper::new(display_info.clone());

        let transparent_texture = load_texture(&gl, transparent_image().into())?;

        let mut renderer = Self {
            gl,
            program,
            vao,
            vbo,
            eab,
            time_started: 0,
            animation_time: 300,
            old_wallpaper,
            current_wallpaper,
            display_info,
            transparent_texture,
            animation_fit_changed: false,
        };

        renderer.load_wallpaper(image, BackgroundMode::Stretch)?;

        Ok(renderer)
    }

    pub fn check_error(&self, msg: &str) -> Result<()> {
        unsafe {
            gl_check!(self.gl, msg);
        }
        Ok(())
    }

    pub unsafe fn draw(&mut self, time: u32, mode: BackgroundMode) -> Result<()> {
        self.gl.Clear(gl::COLOR_BUFFER_BIT);
        self.check_error("clearing the screen")?;

        let elapsed = time - self.time_started;
        let mut progress = (elapsed as f32 / self.animation_time as f32).min(1.0);

        match mode {
            BackgroundMode::Stretch | BackgroundMode::Fill | BackgroundMode::Tile => {}
            BackgroundMode::Fit => {
                if progress > 0.5 && !self.animation_fit_changed {
                    self.gl.ActiveTexture(gl::TEXTURE0);
                    self.check_error("activating gl::TEXTURE0")?;
                    self.gl
                        .BindTexture(gl::TEXTURE_2D, self.transparent_texture);
                    self.gl.ActiveTexture(gl::TEXTURE1);
                    self.check_error("activating gl::TEXTURE0")?;
                    self.current_wallpaper.bind(&self.gl)?;

                    self.animation_fit_changed = true;
                    // This will recalculate the vertices
                    self.set_mode(mode, true)?;
                }
                if progress < 1.0 {
                    progress = (progress % 0.5) * 2.0;
                }
            }
        }

        let loc = self
            .gl
            .GetUniformLocation(self.program, b"u_progress\0".as_ptr() as *const _);
        self.check_error("getting the uniform location")?;
        self.gl.Uniform1f(loc, progress);
        self.check_error("calling Uniform1i")?;

        self.gl
            .DrawElements(gl::TRIANGLES, 6, gl::UNSIGNED_INT, std::ptr::null());
        self.check_error("drawing the triangles")?;

        Ok(())
    }

    pub fn load_wallpaper(&mut self, image: DynamicImage, mode: BackgroundMode) -> Result<()> {
        std::mem::swap(&mut self.old_wallpaper, &mut self.current_wallpaper);
        self.current_wallpaper.load_image(&self.gl, image)?;

        match mode {
            BackgroundMode::Stretch | BackgroundMode::Fill | BackgroundMode::Tile => unsafe {
                self.set_mode(mode, false)?;
                self.gl.ActiveTexture(gl::TEXTURE0);
                self.check_error("activating gl::TEXTURE0")?;
                self.old_wallpaper.bind(&self.gl)?;
                self.gl.ActiveTexture(gl::TEXTURE1);
                self.check_error("activating gl::TEXTURE0")?;
                self.current_wallpaper.bind(&self.gl)?;
            },
            BackgroundMode::Fit => unsafe {
                // We don't change the vertices, we still use the previous ones for the first half
                // of the animation
                self.gl.ActiveTexture(gl::TEXTURE0);
                self.check_error("activating gl::TEXTURE0")?;
                self.old_wallpaper.bind(&self.gl)?;
                self.gl.ActiveTexture(gl::TEXTURE1);
                self.check_error("activating gl::TEXTURE0")?;
                self.gl
                    .BindTexture(gl::TEXTURE_2D, self.transparent_texture);
            },
        }

        Ok(())
    }

    pub fn set_mode(
        &mut self,
        mode: BackgroundMode,
        half_animation_for_fit_mode: bool,
    ) -> Result<()> {
        match mode {
            BackgroundMode::Stretch | BackgroundMode::Fill | BackgroundMode::Tile => {
                // The vertex data will be the default in this case
                let vec_coordinates = Coordinates::default_vec_coordinates();
                let current_tex_coord = &self.current_wallpaper.generate_texture_coordinates(mode);
                let old_tex_coord = &self.old_wallpaper.generate_texture_coordinates(mode);

                let vertex_data =
                    get_opengl_point_coordinates(vec_coordinates, current_tex_coord, old_tex_coord);

                unsafe {
                    // Update the vertex buffer
                    self.gl.BufferSubData(
                        gl::ARRAY_BUFFER,
                        0,
                        (vertex_data.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                        vertex_data.as_ptr() as *const _,
                    );
                    self.check_error("buffering the data")?;
                }
            }
            BackgroundMode::Fit => {
                let vec_coordinates = if half_animation_for_fit_mode {
                    self.current_wallpaper
                        .generate_vertices_coordinates_for_fit_mode()
                } else {
                    self.old_wallpaper.generate_texture_coordinates(mode)
                };

                let old_tex_coord = &self.old_wallpaper.generate_texture_coordinates(mode);

                let vertex_data = get_opengl_point_coordinates(
                    vec_coordinates,
                    &Coordinates::default_texture_coordinates(),
                    old_tex_coord,
                );

                unsafe {
                    // Update the vertex buffer
                    self.gl.BufferSubData(
                        gl::ARRAY_BUFFER,
                        0,
                        (vertex_data.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                        vertex_data.as_ptr() as *const _,
                    );
                    self.check_error("buffering the data")?;
                }
            }
        };
        Ok(())
    }

    pub fn start_animation(&mut self, time: u32) {
        self.time_started = time;
        self.animation_fit_changed = false;
    }

    pub fn clear_after_draw(&self) -> Result<()> {
        unsafe {
            // Unbind the framebuffer and renderbuffer before deleting.
            self.gl.BindBuffer(gl::PIXEL_UNPACK_BUFFER, 0);
            self.check_error("unbinding the unpack buffer")?;
            self.gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, 0);
            self.check_error("unbinding the framebuffer")?;
            self.gl.BindRenderbuffer(gl::RENDERBUFFER, 0);
            self.check_error("unbinding the render buffer")?;
        }

        Ok(())
    }

    pub fn resize(&mut self) -> Result<()> {
        let info = self.display_info.borrow();
        unsafe {
            self.gl
                .Viewport(0, 0, info.adjusted_width(), info.adjusted_height());
            self.check_error("resizing the viewport")
        }
    }

    pub(crate) fn is_drawing_animation(&self, time: u32) -> bool {
        time < (self.time_started + self.animation_time)
    }
}

fn get_opengl_point_coordinates(
    vec_coordinates: Coordinates,
    current_tex_coord: &Coordinates,
    old_tex_coord: &Coordinates,
) -> [f32; 24] {
    [
        vec_coordinates.x_left, // top left start
        vec_coordinates.y_top,
        current_tex_coord.x_left,
        current_tex_coord.y_top,
        old_tex_coord.x_left,
        old_tex_coord.y_top,    // top left stop
        vec_coordinates.x_left, // bottom left start
        vec_coordinates.y_bottom,
        current_tex_coord.x_left,
        current_tex_coord.y_bottom,
        old_tex_coord.x_left,
        old_tex_coord.y_bottom,  // bottom left stop
        vec_coordinates.x_right, // bottom right start
        vec_coordinates.y_bottom,
        current_tex_coord.x_right,
        current_tex_coord.y_bottom,
        old_tex_coord.x_right,
        old_tex_coord.y_bottom,  // bottom right stop
        vec_coordinates.x_right, // top right start
        vec_coordinates.y_top,
        current_tex_coord.x_right,
        current_tex_coord.y_top,
        old_tex_coord.x_right,
        old_tex_coord.y_top, // top right // stop
    ]
}

impl Deref for Renderer {
    type Target = gl::Gl;

    fn deref(&self) -> &Self::Target {
        &self.gl
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            self.gl.DeleteTextures(1, &self.current_wallpaper.texture);
            self.gl.DeleteTextures(1, &self.old_wallpaper.texture);
            self.gl.DeleteBuffers(1, &self.eab);
            self.gl.DeleteBuffers(1, &self.vbo);
            self.gl.DeleteBuffers(1, &self.vao);
            self.gl.DeleteProgram(self.program);
        }
    }
}

unsafe fn create_shader(
    gl: &gl::Gl,
    shader: gl::types::GLenum,
    source: &[u8],
) -> Result<gl::types::GLuint> {
    let shader = gl.CreateShader(shader);
    gl_check!(gl, "calling CreateShader");
    gl.ShaderSource(
        shader,
        1,
        [source.as_ptr().cast()].as_ptr(),
        std::ptr::null(),
    );
    gl_check!(gl, "calling Shadersource");
    gl.CompileShader(shader);
    gl_check!(gl, "calling CompileShader");

    let mut status: i32 = 0;
    gl.GetShaderiv(shader, gl::COMPILE_STATUS, &mut status as *mut _);
    gl_check!(gl, "calling GetShaderiv");
    if status == 0 {
        let mut max_length: i32 = 0;
        let mut length: i32 = 0;
        gl.GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut max_length as *mut _);
        gl_check!(gl, "calling GetShaderiv");
        let mut log: Vec<u8> = vec![0; max_length as _];
        gl.GetShaderInfoLog(
            shader,
            max_length,
            &mut length as *mut _,
            log.as_mut_ptr() as _,
        );
        gl_check!(gl, "calling GetShaderInfoLog");
        let log = String::from_utf8(log).unwrap();
        Err(color_eyre::eyre::anyhow!(log))
    } else {
        Ok(shader)
    }
}

fn initialize_objects(
    gl: &gl::Gl,
) -> Result<(gl::types::GLuint, gl::types::GLuint, gl::types::GLuint)> {
    unsafe {
        let mut vao = 0;
        gl.GenVertexArrays(1, &mut vao);
        gl_check!(gl, "generating the vertex array");
        gl.BindVertexArray(vao);
        gl_check!(gl, "binding the vertex array");
        let mut vbo = 0;
        gl.GenBuffers(1, &mut vbo);
        gl_check!(gl, "generating the vbo buffer");
        gl.BindBuffer(gl::ARRAY_BUFFER, vbo);
        gl_check!(gl, "binding the vbo buffer");
        let vertex_data: Vec<f32> = vec![0.0; 24 as _];
        gl.BufferData(
            gl::ARRAY_BUFFER,
            (vertex_data.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
            vertex_data.as_ptr() as *const _,
            gl::STATIC_DRAW,
        );
        gl_check!(gl, "buffering the data");

        let mut eab = 0;
        gl.GenBuffers(1, &mut eab);
        gl_check!(gl, "generating the eab buffer");
        gl.BindBuffer(gl::ELEMENT_ARRAY_BUFFER, eab);
        gl_check!(gl, "binding the eab buffer");
        // We load the elements array buffer once, it's the same for each wallpaper
        const INDICES: [gl::types::GLuint; 6] = [0, 1, 2, 2, 3, 0];
        gl.BufferData(
            gl::ELEMENT_ARRAY_BUFFER,
            (INDICES.len() * std::mem::size_of::<gl::types::GLuint>()) as gl::types::GLsizeiptr,
            INDICES.as_ptr() as *const _,
            gl::STATIC_DRAW,
        );
        gl_check!(gl, "buffering the data");

        const POS_ATTRIB: i32 = 0;
        const TEX_ATTRIB: i32 = 1;
        const TEX2_ATTRIB: i32 = 2;
        gl.VertexAttribPointer(
            POS_ATTRIB as gl::types::GLuint,
            2,
            gl::FLOAT,
            0,
            6 * std::mem::size_of::<f32>() as gl::types::GLsizei,
            std::ptr::null(),
        );
        gl_check!(gl, "setting the position attribute for the vertex");
        gl.EnableVertexAttribArray(POS_ATTRIB as gl::types::GLuint);
        gl_check!(gl, "enabling the position attribute for the vertex");
        gl.VertexAttribPointer(
            TEX_ATTRIB as gl::types::GLuint,
            2,
            gl::FLOAT,
            0,
            6 * std::mem::size_of::<f32>() as gl::types::GLsizei,
            (2 * std::mem::size_of::<f32>()) as *const () as *const _,
        );
        gl_check!(gl, "setting the texture attribute for the vertex");
        gl.EnableVertexAttribArray(TEX_ATTRIB as gl::types::GLuint);
        gl_check!(gl, "enabling the texture attribute for the vertex");
        gl.VertexAttribPointer(
            TEX2_ATTRIB as gl::types::GLuint,
            2,
            gl::FLOAT,
            0,
            6 * std::mem::size_of::<f32>() as gl::types::GLsizei,
            (4 * std::mem::size_of::<f32>()) as *const () as *const _,
        );
        gl_check!(gl, "setting the texture attribute for the vertex");
        gl.EnableVertexAttribArray(TEX2_ATTRIB as gl::types::GLuint);
        gl_check!(gl, "enabling the texture attribute for the vertex");

        Ok((vao, vbo, eab))
    }
}

const VERTEX_SHADER_SOURCE: &[u8] = b"
#version 320 es
precision mediump float;

layout (location = 0) in vec2 aPosition;
layout (location = 1) in vec2 aCurrentTexCoord;
layout (location = 2) in vec2 aOldTexCoord;

out vec2 v_old_texcoord;
out vec2 v_current_texcoord;

void main() {
    gl_Position = vec4(aPosition, 1.0, 1.0);
    v_current_texcoord = aCurrentTexCoord;
    v_old_texcoord = aOldTexCoord;
}
\0";

const FRAGMENT_SHADER_SOURCE: &[u8] = b"
#version 320 es
precision mediump float;
out vec4 FragColor;

in vec2 v_old_texcoord;
in vec2 v_current_texcoord;

layout(location = 0) uniform sampler2D u_old_texture;
layout(location = 1) uniform sampler2D u_current_texture;

layout(location = 2) uniform float u_progress;

void main() {
    FragColor = mix(texture(u_old_texture, v_old_texcoord), texture(u_current_texture, v_current_texcoord), u_progress);
}
\0";
