use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::window::{WindowCloseRequested, WindowResized};
use bevy_replicon::client_disconnected;
use bevy_replicon::prelude::*;
use bevy_replicon::renet::transport::{
    ClientAuthentication, NetcodeClientTransport, NetcodeServerTransport, ServerAuthentication,
    ServerConfig,
};
use bevy_replicon::renet::{ConnectionConfig, ServerEvent};
use clap::Parser;
use rand::seq::SliceRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, SystemTime};

const CELL_SIZE: Vec2 = Vec2::new(0.152625, 0.1538);
const PAWN_SIZE: f32 = 0.8;
const BOARD_ASPECT_RATIO: f32 = 1600.0 / 1550.0;
const BOARD_PADDING: f32 = 0.2;
const BOARD_SIZE: usize = 6;
const MOVE_ANIM_DURATION: Duration = Duration::from_millis(500);
const COLORS: [Color; 4] = [Color::RED, Color::GREEN, Color::BLUE, Color::YELLOW];
const EXPLOSION_FRAMES: usize = 22;
const EXPLOSION_FRAME_TIME: Duration = Duration::from_nanos(
    Duration::from_millis(500).subsec_nanos() as u64 / EXPLOSION_FRAMES as u64,
);
const ITEMS_TO_WIN: usize = 5;

fn main() {
    let cli = Cli::parse();
    let mut app = App::new();
    if matches!(cli, Cli::Server { .. }) {
        app.add_plugins((bevy::log::LogPlugin::default(), MinimalPlugins));
    } else {
        app.add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Labyrinth".into(),
                ..default()
            }),
            close_when_requested: false,
            ..default()
        }));
        app.insert_resource(ClearColor(Color::rgb(0.0, 0.0, 0.1)));
    }
    app.insert_resource(cli);
    app.add_plugins((ReplicationPlugins, LabyrinthPlugin));
    app.run();
}

struct LabyrinthPlugin;

impl Plugin for LabyrinthPlugin {
    fn build(&self, app: &mut App) {
        app.replicate::<Player>();
        app.replicate::<Dice>();
        app.add_server_event::<GameState>(EventType::Ordered);
        app.add_server_event::<TurnPhase>(EventType::Ordered);
        app.add_server_event::<CurrentTurn>(EventType::Ordered);
        app.add_server_event::<PlayerStartMoveAnimation>(EventType::Ordered);
        app.add_client_event::<DiceRollRequest>(EventType::Ordered);
        app.add_client_event::<MoveRequest>(EventType::Ordered);
        app.add_state::<GameState>();
        app.add_state::<TurnPhase>();
        app.init_resource::<CurrentTurn>();
        app.add_systems(Startup, Self::init.map(Result::unwrap));
        app.add_systems(
            Update,
            (
                // client systems
                (
                    Self::client_handle_keyboard_input.run_if(in_state(GameState::InGame)),
                    Self::client_on_disconnected.run_if(client_disconnected()),
                    Self::client_on_window_resize,
                    Self::client_on_window_close_requested,
                    Self::client_update_player_anim,
                    Self::client_update_explosion_anim,
                )
                    .run_if(resource_exists::<RenetClient>()),
                // server systems
                (Self::server_on_events,).run_if(has_authority()),
            ),
        );
        app.add_systems(
            PreUpdate,
            (
                // client on-rep systems
                (
                    Self::client_on_rep_game_state,
                    Self::client_on_rep_player,
                    Self::client_update_player_data,
                    Self::client_on_rep_dice,
                    Self::client_on_dice_value_change,
                )
                    .run_if(resource_exists::<RenetClient>())
                    .after(ClientSet::Receive),
                // server on-rep systems
                (Self::server_receive_requests,)
                    .run_if(has_authority())
                    .run_if(in_state(GameState::InGame))
                    .after(ServerSet::Receive),
            ),
        );
    }
}

impl LabyrinthPlugin {
    fn init(
        mut commands: Commands,
        window: Query<&Window>,
        cli: Res<Cli>,
        network_channels: Res<NetworkChannels>,
        mut texture_atlases: Option<ResMut<Assets<TextureAtlas>>>,
        assets: Option<Res<AssetServer>>,
    ) -> Result<(), Box<dyn Error>> {
        match *cli {
            Cli::Server {
                port,
                max_players,
                tiles,
            } => {
                info!("Starting server on port {port} with {max_players} players");
                let server_channels_config = network_channels.get_server_configs();
                let client_channels_config = network_channels.get_client_configs();

                let server = RenetServer::new(ConnectionConfig {
                    server_channels_config,
                    client_channels_config,
                    ..default()
                });

                let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
                let public_addr = SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), port);
                let socket = UdpSocket::bind(public_addr)?;
                let server_config = ServerConfig {
                    current_time,
                    max_clients: max_players as usize,
                    protocol_id: PROTOCOL_ID,
                    authentication: ServerAuthentication::Unsecure,
                    public_addresses: vec![public_addr],
                };
                let transport = NetcodeServerTransport::new(server_config, socket)?;

                commands.spawn(DiceBundle::default());

                commands.insert_resource(MaxPlayers(max_players as usize));
                commands.insert_resource(server);
                commands.insert_resource(transport);
                commands.insert_resource(Maze::generate(tiles));
                commands.init_resource::<AvailableItems>();
            }
            Cli::Client { ip, port } => {
                info!("Connecting to {ip}:{port}");
                let assets = assets.unwrap();

                let server_channels_config = network_channels.get_server_configs();
                let client_channels_config = network_channels.get_client_configs();

                let client = RenetClient::new(ConnectionConfig {
                    server_channels_config,
                    client_channels_config,
                    ..default()
                });

                let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
                let client_id = current_time.as_millis() as u64;
                let server_addr = SocketAddr::new(ip, port);
                let socket = UdpSocket::bind((IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0))?;
                let authentication = ClientAuthentication::Unsecure {
                    client_id,
                    protocol_id: PROTOCOL_ID,
                    server_addr,
                    user_data: None,
                };
                let transport = NetcodeClientTransport::new(current_time, authentication, socket)?;

                commands.insert_resource(client);
                commands.insert_resource(transport);

                let window = window.single();

                commands.insert_resource(WindowSize(Vec2::new(window.width(), window.height())));

                commands.spawn(Camera2dBundle::default());
                commands.spawn((
                    SpriteBundle {
                        transform: Transform {
                            translation: Vec3::NEG_Z,
                            ..default()
                        },
                        sprite: Sprite {
                            custom_size: Some(Self::calc_board_size(Vec2::new(
                                window.width(),
                                window.height(),
                            ))),
                            ..default()
                        },
                        texture: assets.load("background.png"),
                        ..default()
                    },
                    Background,
                ));

                let dice_texture = assets.load("dice.png");
                let dice_atlas =
                    TextureAtlas::from_grid(dice_texture, Vec2::splat(415.0), 2, 2, None, None);
                let dice_atlas_handle = texture_atlases.as_mut().unwrap().add(dice_atlas);

                let explosion_texture = assets.load("explosion.png");
                let explosion_atlas =
                    TextureAtlas::from_grid(explosion_texture, Vec2::splat(64.0), 8, 3, None, None);
                let explosion_atlas_handle = texture_atlases.as_mut().unwrap().add(explosion_atlas);

                let background_texture = assets.load("background.png");
                // 250x237 + 110x123
                // 146x126
                let items_atlas = TextureAtlas::from_grid(
                    background_texture,
                    Vec2::new(146.0, 126.0),
                    BOARD_SIZE,
                    BOARD_SIZE,
                    Some(Vec2::new(104.0, 111.0)),
                    Some(Vec2::new(110.0, 123.0)),
                );
                let items_atlas_handle = texture_atlases.as_mut().unwrap().add(items_atlas);

                commands.insert_resource(TextureAtlases {
                    dice: dice_atlas_handle,
                    explosion: explosion_atlas_handle,
                    items: items_atlas_handle,
                });
            }
        }
        Ok(())
    }

    fn client_on_disconnected(mut app_exit_events: ResMut<Events<AppExit>>) {
        info!("Client disconnected!");
        app_exit_events.send(AppExit);
    }

    fn client_on_window_resize(
        mut events: EventReader<WindowResized>,
        mut window_size: ResMut<WindowSize>,
        current_turn: Res<CurrentTurn>,
        mut background: Query<
            &mut Sprite,
            (
                With<Background>,
                Without<Player>,
                Without<Dice>,
                Without<ItemDisplay>,
            ),
        >,
        mut players: Query<(
            &Player,
            &mut Transform,
            Option<&PlayerMoveAnimation>,
            &mut Sprite,
        )>,
        mut dice: Query<(&mut Transform, &mut TextureAtlasSprite), (With<Dice>, Without<Player>)>,
        mut item_displays: Query<
            (&ItemDisplay, &mut Transform, &mut TextureAtlasSprite),
            (Without<Dice>, Without<Player>),
        >,
    ) {
        let mut background = background.single_mut();
        for event in events.read() {
            window_size.0 = Vec2::new(event.width, event.height);
            let board_size = Self::calc_board_size(window_size.0);
            background.custom_size = Some(board_size);
            for (player, mut player_transform, anim, mut player_sprite) in players.iter_mut() {
                player_transform.translation =
                    Self::calc_player_pos(player.prev_coords, player.coords, anim, board_size)
                        .extend(0.0);
                player_sprite.custom_size =
                    Some(Vec2::splat(board_size.y * CELL_SIZE.y * PAWN_SIZE));
            }
            for (mut dice_transform, mut dice_sprite) in dice.iter_mut() {
                dice_transform.translation =
                    Self::calc_dice_pos(window_size.0, board_size, current_turn.0).extend(0.0);
                dice_sprite.custom_size = Some(Self::calc_dice_size(window_size.0, board_size));
            }
            for (item_display, mut item_display_transform, mut item_display_sprite) in
                item_displays.iter_mut()
            {
                item_display_transform.translation =
                    Self::calc_item_display_pos(window_size.0, board_size, item_display)
                        .extend(1.0);
                item_display_sprite.custom_size =
                    Some(Self::calc_item_display_size(window_size.0, board_size));
            }
        }
    }

    fn client_on_window_close_requested(
        mut events: EventReader<WindowCloseRequested>,
        mut client: ResMut<RenetClient>,
        mut app_exit_events: ResMut<Events<AppExit>>,
    ) {
        for _ in events.read() {
            client.disconnect();
            app_exit_events.send(AppExit);
        }
    }

    fn client_handle_keyboard_input(
        keys: Res<Input<KeyCode>>,
        not_moving_me: Query<&Player, (With<Me>, Without<PlayerMoveAnimation>)>,
        current_turn: Res<CurrentTurn>,
        turn_phase: Res<State<TurnPhase>>,
        mut roll_requests: EventWriter<DiceRollRequest>,
        mut move_requests: EventWriter<MoveRequest>,
    ) {
        let Ok(not_moving_me) = not_moving_me.get_single() else {
            return;
        };
        if not_moving_me.player_number != current_turn.0 {
            return;
        }
        match turn_phase.get() {
            TurnPhase::Rolling => {
                if keys.just_pressed(KeyCode::Space) {
                    roll_requests.send(DiceRollRequest);
                }
            }
            TurnPhase::Moving { .. } => {
                if keys.just_pressed(KeyCode::Up) || keys.just_pressed(KeyCode::W) {
                    move_requests.send(MoveRequest::Up);
                }
                if keys.just_pressed(KeyCode::Down) || keys.just_pressed(KeyCode::S) {
                    move_requests.send(MoveRequest::Down);
                }
                if keys.just_pressed(KeyCode::Left) || keys.just_pressed(KeyCode::A) {
                    move_requests.send(MoveRequest::Left);
                }
                if keys.just_pressed(KeyCode::Right) || keys.just_pressed(KeyCode::D) {
                    move_requests.send(MoveRequest::Right);
                }
            }
        }
    }

    fn client_on_rep_game_state(
        mut commands: Commands,
        mut game_state_events: EventReader<GameState>,
        mut turn_phase_events: EventReader<TurnPhase>,
        mut current_turn_events: EventReader<CurrentTurn>,
        mut start_move_animation_events: EventReader<PlayerStartMoveAnimation>,
        mut game_state: ResMut<NextState<GameState>>,
        mut turn_phase: ResMut<NextState<TurnPhase>>,
        mut current_turn: ResMut<CurrentTurn>,
        window_size: Res<WindowSize>,
        players: Query<(Entity, &Player)>,
        mut dice: Query<&mut Transform, With<Dice>>,
    ) {
        if let Some(state) = game_state_events.read().last() {
            game_state.set(*state);
        }
        if let Some(phase) = turn_phase_events.read().last() {
            turn_phase.set(*phase);
        }
        if let Some(turn) = current_turn_events.read().last() {
            *current_turn = *turn;
            dice.single_mut().translation =
                Self::calc_dice_pos(window_size.0, Self::calc_board_size(window_size.0), turn.0)
                    .extend(0.0);
        }
        for event in start_move_animation_events.read() {
            if let Some((entity_id, _)) = players
                .iter()
                .find(|(_, player)| player.client_id == event.client_id)
            {
                commands.entity(entity_id).insert(PlayerMoveAnimation {
                    fail: event.fail,
                    move_to: event.move_to,
                    ..default()
                });
            }
        }
    }

    fn client_on_rep_player(
        mut commands: Commands,
        spawned_players: Query<(Entity, &Player), Added<Player>>,
        mut items_query: Query<(Entity, &ItemDisplay, &mut TextureAtlasSprite)>,
        transport: Res<NetcodeClientTransport>,
        window_size: Res<WindowSize>,
        assets: Res<AssetServer>,
        atlases: Res<TextureAtlases>,
    ) {
        for (id, player) in spawned_players.iter() {
            info!("Replicated player: {}", player.player_number);
            if player.player_number >= 4 {
                commands.entity(id).despawn();
                continue;
            }

            let board_size = Self::calc_board_size(window_size.0);

            commands.entity(id).insert(SpriteBundle {
                sprite: Sprite {
                    color: COLORS[player.player_number],
                    custom_size: Some(Vec2::splat(board_size.y * CELL_SIZE.y * PAWN_SIZE)),
                    ..default()
                },
                texture: assets.load("pawn.png"),
                transform: Transform {
                    translation: Self::board_pos_to_pos(player.coords, board_size).extend(0.0),
                    ..default()
                },
                ..default()
            });
            if player.client_id == transport.client_id() {
                commands.entity(id).insert(Me);
            }
            Self::sync_player_items(
                &mut commands,
                player,
                &mut items_query,
                &*atlases,
                window_size.0,
            );
        }
    }

    fn client_update_player_data(
        mut commands: Commands,
        mut players: Query<
            (&Player, &mut Transform, Option<&PlayerMoveAnimation>),
            Changed<Player>,
        >,
        window_size: Res<WindowSize>,
        mut items_query: Query<(Entity, &ItemDisplay, &mut TextureAtlasSprite)>,
        atlases: Res<TextureAtlases>,
    ) {
        for (player, mut transform, anim) in players.iter_mut() {
            transform.translation = Self::calc_player_pos(
                player.prev_coords,
                player.coords,
                anim,
                Self::calc_board_size(window_size.0),
            )
            .extend(0.0);
            Self::sync_player_items(
                &mut commands,
                player,
                &mut items_query,
                &*atlases,
                window_size.0,
            );
        }
    }

    fn sync_player_items(
        commands: &mut Commands,
        player: &Player,
        items_query: &mut Query<(Entity, &ItemDisplay, &mut TextureAtlasSprite)>,
        atlases: &TextureAtlases,
        window_size: Vec2,
    ) {
        let mut first_unspawned_index = 0;
        let mut found_target = false;
        for (entity_id, item_display, mut sprite) in items_query.iter_mut() {
            if item_display.player_index != player.player_number {
                continue;
            }
            match item_display.position {
                ItemDisplayPosition::Achieved(index) => {
                    first_unspawned_index = first_unspawned_index.max(index + 1);
                    if index >= player.achieved_items.len() {
                        commands.entity(entity_id).despawn();
                    } else {
                        sprite.index = player.achieved_items[index].atlas_index();
                    }
                }
                ItemDisplayPosition::Target => {
                    found_target = true;
                    if let Some(target) = player.target_item {
                        sprite.index = target.atlas_index();
                    } else {
                        commands.entity(entity_id).despawn();
                    }
                }
            }
        }

        let board_size = Self::calc_board_size(window_size);

        let mut spawn_item = |item: Item, item_display: ItemDisplay| {
            let position = Self::calc_item_display_pos(window_size, board_size, &item_display);
            commands.spawn(ItemDisplayBundle {
                item: item_display,
                sprite: SpriteSheetBundle {
                    transform: Transform {
                        translation: position.extend(1.0),
                        ..default()
                    },
                    sprite: TextureAtlasSprite {
                        custom_size: Some(Self::calc_item_display_size(window_size, board_size)),
                        index: item.atlas_index(),
                        ..default()
                    },
                    texture_atlas: atlases.items.clone(),
                    ..default()
                },
            });
        };

        if !found_target {
            if let Some(target) = player.target_item {
                spawn_item(
                    target,
                    ItemDisplay {
                        player_index: player.player_number,
                        position: ItemDisplayPosition::Target,
                    },
                );
            }
        }

        if first_unspawned_index < player.achieved_items.len() {
            for index in first_unspawned_index..player.achieved_items.len() {
                spawn_item(
                    player.achieved_items[index],
                    ItemDisplay {
                        player_index: player.player_number,
                        position: ItemDisplayPosition::Achieved(index),
                    },
                );
            }
        }
    }

    fn client_update_player_anim(
        mut commands: Commands,
        mut players: Query<(
            Entity,
            &mut Player,
            &mut PlayerMoveAnimation,
            &mut Transform,
        )>,
        time: Res<Time>,
        window_size: Res<WindowSize>,
        atlases: Res<TextureAtlases>,
    ) {
        for (id, mut player, mut move_anim, mut transform) in players.iter_mut() {
            let old_time = move_anim.time;
            move_anim.time += time.delta();

            if move_anim.fail
                && Self::get_anim_delta(old_time) < 0.5
                && Self::get_anim_delta(move_anim.time) >= 0.5
            {
                commands.spawn(ExplosionBundle {
                    sprite: SpriteSheetBundle {
                        transform: Transform {
                            translation: transform.translation.xy().extend(1.0),
                            ..default()
                        },
                        sprite: TextureAtlasSprite {
                            custom_size: Some(Vec2::splat(
                                Self::calc_board_size(window_size.0).y * CELL_SIZE.y * PAWN_SIZE,
                            )),
                            ..default()
                        },
                        texture_atlas: atlases.explosion.clone(),
                        ..default()
                    },
                    ..default()
                });
            }

            if move_anim.time > MOVE_ANIM_DURATION {
                move_anim.time = MOVE_ANIM_DURATION;
                commands.entity(id).remove::<PlayerMoveAnimation>();
                player.prev_coords = player.coords;
            }
            transform.translation = Self::calc_player_pos(
                player.prev_coords,
                player.coords,
                Some(&*move_anim),
                Self::calc_board_size(window_size.0),
            )
            .extend(0.0);
        }
    }

    fn get_anim_delta(anim_time: Duration) -> f32 {
        (anim_time.as_secs_f32() / MOVE_ANIM_DURATION.as_secs_f32() * std::f32::consts::FRAC_PI_2)
            .sin()
    }

    fn calc_player_pos(
        prev_coords: IVec2,
        coords: IVec2,
        anim: Option<&PlayerMoveAnimation>,
        board_size: Vec2,
    ) -> Vec2 {
        if let Some(anim) = anim {
            let anim_delta = Self::get_anim_delta(anim.time);
            if anim.fail && anim_delta >= 0.75 {
                Self::board_pos_to_pos(coords, board_size)
            } else {
                let prev_pos = Self::board_pos_to_pos(prev_coords, board_size);
                let to_pos = Self::board_pos_to_pos(anim.move_to, board_size);
                prev_pos + (to_pos - prev_pos) * anim_delta
            }
        } else {
            Self::board_pos_to_pos(coords, board_size)
        }
    }

    fn client_on_rep_dice(
        mut commands: Commands,
        spawned_dice: Query<(Entity, &Dice), Added<Dice>>,
        atlases: Res<TextureAtlases>,
        window_size: Res<WindowSize>,
        current_turn: Res<CurrentTurn>,
    ) {
        for (id, dice) in spawned_dice.iter() {
            let board_size = Self::calc_board_size(window_size.0);
            commands.entity(id).insert(SpriteSheetBundle {
                transform: Transform {
                    translation: Self::calc_dice_pos(window_size.0, board_size, current_turn.0)
                        .extend(0.0),
                    ..default()
                },
                sprite: TextureAtlasSprite {
                    custom_size: Some(Self::calc_dice_size(window_size.0, board_size)),
                    index: (dice.value.clamp(1, 4) - 1) as usize,
                    ..default()
                },
                texture_atlas: atlases.dice.clone(),
                ..default()
            });
        }
    }

    fn client_on_dice_value_change(
        mut dice: Query<(&Dice, &mut TextureAtlasSprite), Changed<Dice>>,
    ) {
        for (dice, mut sprite) in dice.iter_mut() {
            sprite.index = (dice.value.clamp(1, 4) - 1) as usize;
        }
    }

    fn board_pos_to_pos(board_pos: IVec2, board_size: Vec2) -> Vec2 {
        (board_pos.as_vec2() - Vec2::splat(2.5)) * board_size * CELL_SIZE
    }

    fn calc_board_size(window_size: Vec2) -> Vec2 {
        let adjusted_window_size = window_size * Vec2::new(1.0 / BOARD_ASPECT_RATIO, 1.0);
        Vec2::splat(
            adjusted_window_size
                .min_element()
                .min(adjusted_window_size.max_element() * (1.0 - BOARD_PADDING)),
        ) * Vec2::new(BOARD_ASPECT_RATIO, 1.0)
    }

    fn calc_dice_pos(window_size: Vec2, board_size: Vec2, turn: usize) -> Vec2 {
        let margin = (window_size - board_size).max_element() * 0.5;
        Vec2::new(
            if turn / 2 == 0 {
                margin - window_size.x
            } else {
                window_size.x - margin
            },
            if turn % 2 == 0 {
                margin - window_size.y
            } else {
                window_size.y - margin
            },
        ) * 0.5
    }

    fn calc_dice_size(window_size: Vec2, board_size: Vec2) -> Vec2 {
        let margin = (window_size - board_size).max_element() * 0.5;
        Vec2::splat(margin * 0.8)
    }

    fn calc_item_display_pos(
        window_size: Vec2,
        board_size: Vec2,
        item_display: &ItemDisplay,
    ) -> Vec2 {
        let item_size = Self::calc_item_display_size(window_size, board_size);
        let dice_size = Self::calc_dice_size(window_size, board_size);
        let dice_pos = Self::calc_dice_pos(window_size, board_size, item_display.player_index);
        let x = match item_display.position {
            ItemDisplayPosition::Achieved(index) => {
                dice_pos.x - dice_size.x * 0.5 + (index + 2) as f32 * 0.5 * item_size.x
            }
            ItemDisplayPosition::Target => dice_pos.x + dice_size.x * 0.5 - item_size.x,
        };
        Vec2::new(x, dice_pos.y)
    }

    fn calc_item_display_size(window_size: Vec2, board_size: Vec2) -> Vec2 {
        Self::calc_dice_size(window_size, board_size) * 0.2
    }

    fn server_receive_requests(
        mut current_turn: ResMut<CurrentTurn>,
        mut current_turn_writer: EventWriter<ToClients<CurrentTurn>>,
        turn_phase: Res<State<TurnPhase>>,
        player_count: Res<MaxPlayers>,
        mut next_turn_phase: ResMut<NextState<TurnPhase>>,
        mut turn_phase_writer: EventWriter<ToClients<TurnPhase>>,
        mut move_requests: EventReader<FromClient<MoveRequest>>,
        mut roll_requests: EventReader<FromClient<DiceRollRequest>>,
        mut players: Query<&mut Player>,
        mut player_start_move_anim_writer: EventWriter<ToClients<PlayerStartMoveAnimation>>,
        mut dice: Query<&mut Dice, Without<Player>>,
        maze: Res<Maze>,
        mut available_items: ResMut<AvailableItems>,
        mut next_game_state: ResMut<NextState<GameState>>,
        mut game_state_writer: EventWriter<ToClients<GameState>>,
    ) {
        let mut turn_phase = *turn_phase.get();
        for FromClient { client_id, .. } in roll_requests.read() {
            if turn_phase != TurnPhase::Rolling {
                continue;
            }
            if players.iter().any(|player| {
                player.client_id == client_id.raw() && player.player_number == current_turn.0
            }) {
                dice.single_mut().value =
                    *[1, 2, 2, 3, 3, 4].choose(&mut rand::thread_rng()).unwrap();
                next_turn_phase.set(TurnPhase::Moving { steps_taken: 0 });
                turn_phase_writer.send(ToClients {
                    mode: SendMode::Broadcast,
                    event: TurnPhase::Moving { steps_taken: 0 },
                });
                turn_phase = TurnPhase::Moving { steps_taken: 0 }
            }
        }

        if let TurnPhase::Moving { steps_taken } = turn_phase {
            let mut new_steps_taken = steps_taken;
            let dice_value = dice.single().value;
            for FromClient { client_id, event } in move_requests.read() {
                if new_steps_taken >= dice_value {
                    continue;
                }
                let Some(mut player) = players.iter_mut().find(|player| {
                    player.client_id == client_id.raw() && player.player_number == current_turn.0
                }) else {
                    continue;
                };
                let next_pos = player.coords + event.delta();
                if !(0..BOARD_SIZE as i32).contains(&next_pos.x)
                    || !(0..BOARD_SIZE as i32).contains(&next_pos.y)
                {
                    continue;
                }

                player.prev_coords = player.coords;
                if maze.is_blocked(player.coords, next_pos) {
                    player_start_move_anim_writer.send(ToClients {
                        mode: SendMode::Broadcast,
                        event: PlayerStartMoveAnimation {
                            client_id: player.client_id,
                            fail: true,
                            move_to: next_pos,
                        },
                    });
                    player.coords = Self::get_player_start_coords(player.player_number);
                    new_steps_taken = dice_value;
                } else {
                    player_start_move_anim_writer.send(ToClients {
                        mode: SendMode::Broadcast,
                        event: PlayerStartMoveAnimation {
                            client_id: player.client_id,
                            fail: false,
                            move_to: next_pos,
                        },
                    });
                    player.coords = next_pos;
                    new_steps_taken += 1;

                    if let Some(target_item) = player.target_item {
                        if player.coords == target_item.coords() {
                            player.achieved_items.push(target_item);
                            if player.achieved_items.len() >= ITEMS_TO_WIN {
                                player.target_item = None;
                                next_game_state.set(GameState::Win);
                                game_state_writer.send(ToClients {
                                    mode: SendMode::Broadcast,
                                    event: GameState::Win,
                                });
                                return;
                            } else {
                                player.target_item = available_items.take_random();
                            }
                        }
                    }
                }
            }

            if new_steps_taken != steps_taken {
                if new_steps_taken >= dice_value {
                    current_turn.0 = (current_turn.0 + 1) % player_count.0;
                    current_turn_writer.send(ToClients {
                        mode: SendMode::Broadcast,
                        event: *current_turn,
                    });

                    next_turn_phase.set(TurnPhase::Rolling);
                    turn_phase_writer.send(ToClients {
                        mode: SendMode::Broadcast,
                        event: TurnPhase::Rolling,
                    });
                    turn_phase = TurnPhase::Rolling;
                } else {
                    next_turn_phase.set(TurnPhase::Moving {
                        steps_taken: new_steps_taken,
                    });
                    turn_phase_writer.send(ToClients {
                        mode: SendMode::Broadcast,
                        event: TurnPhase::Moving {
                            steps_taken: new_steps_taken,
                        },
                    });
                    turn_phase = TurnPhase::Moving {
                        steps_taken: new_steps_taken,
                    };
                }
            }
        }
    }

    fn client_update_explosion_anim(
        mut commands: Commands,
        mut explosions: Query<(Entity, &mut Explosion, &mut TextureAtlasSprite)>,
        time: Res<Time>,
    ) {
        for (entity_id, mut explosion, mut sprite) in explosions.iter_mut() {
            explosion.time += time.delta();
            let frame =
                (explosion.time.as_secs_f32() / EXPLOSION_FRAME_TIME.as_secs_f32()) as usize;
            if frame >= EXPLOSION_FRAMES {
                commands.entity(entity_id).despawn();
            } else {
                sprite.index = frame;
            }
        }
    }

    fn server_on_events(
        mut commands: Commands,
        mut events: EventReader<ServerEvent>,
        player_counter: Query<(), With<Player>>,
        max_players: Res<MaxPlayers>,
        mut available_items: ResMut<AvailableItems>,
        mut game_state: ResMut<NextState<GameState>>,
        mut game_state_writer: EventWriter<ToClients<GameState>>,
        mut app_exit_events: ResMut<Events<AppExit>>,
    ) {
        for event in events.read() {
            match event {
                ServerEvent::ClientConnected { client_id } => {
                    info!("Client {client_id} connected");
                    let num_existing_players = player_counter.iter().count();
                    let coords = Self::get_player_start_coords(num_existing_players);
                    commands.spawn(PlayerBundle {
                        player: Player {
                            client_id: client_id.raw(),
                            coords,
                            prev_coords: coords,
                            player_number: num_existing_players,
                            target_item: available_items.take_random(),
                            ..default()
                        },
                        ..default()
                    });
                    if num_existing_players + 1 == max_players.0 {
                        game_state.set(GameState::InGame);
                        game_state_writer.send(ToClients {
                            mode: SendMode::Broadcast,
                            event: GameState::InGame,
                        });
                    }
                }
                ServerEvent::ClientDisconnected { client_id, reason } => {
                    info!("Client {client_id} disconnected: {reason}");
                    info!("Stopping server");
                    app_exit_events.send(AppExit);
                }
            }
        }
    }

    fn get_player_start_coords(player_number: usize) -> IVec2 {
        IVec2::new(
            (player_number / 2 * (BOARD_SIZE - 1)) as i32,
            (player_number % 2 * (BOARD_SIZE - 1)) as i32,
        )
    }
}

const PROTOCOL_ID: u64 = 0;
const DEFAULT_PORT: u16 = 5000;

#[derive(Parser, PartialEq, Resource)]
enum Cli {
    Server {
        #[arg(short, long, default_value_t = DEFAULT_PORT, value_parser = clap::value_parser!(u16).range(1024..))]
        port: u16,
        #[arg(short, long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=4))]
        max_players: u8,
        #[arg(short, long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(15..=20))]
        tiles: u8,
    },
    Client {
        #[arg(short, long, default_value_t = Ipv4Addr::LOCALHOST.into())]
        ip: IpAddr,
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
}

#[derive(Component)]
struct Background;

#[derive(Resource)]
struct MaxPlayers(usize);

#[derive(Resource)]
struct WindowSize(Vec2);

#[derive(Event, Resource, Copy, Clone, Default, Serialize, Deserialize)]
struct CurrentTurn(usize);

#[derive(Component, Serialize, Deserialize, Default)]
struct Player {
    client_id: u64,
    coords: IVec2,
    prev_coords: IVec2,
    player_number: usize,
    target_item: Option<Item>,
    achieved_items: Vec<Item>,
}

#[derive(Bundle, Default)]
struct PlayerBundle {
    player: Player,
    replication: Replication,
}

#[derive(Component)]
struct Me;

#[derive(Component, Default)]
struct PlayerMoveAnimation {
    time: Duration,
    fail: bool,
    move_to: IVec2,
}

#[derive(Event, Serialize, Deserialize)]
struct PlayerStartMoveAnimation {
    client_id: u64,
    fail: bool,
    move_to: IVec2,
}

#[derive(Component, Default)]
struct Explosion {
    time: Duration,
}

#[derive(Bundle, Default)]
struct ExplosionBundle {
    explosion: Explosion,
    sprite: SpriteSheetBundle,
}

#[derive(
    Event, States, Copy, Clone, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize,
)]
enum GameState {
    #[default]
    WaitingPlayers,
    InGame,
    Win,
}

#[derive(
    Event, States, Copy, Clone, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize,
)]
enum TurnPhase {
    #[default]
    Rolling,
    Moving {
        steps_taken: u8,
    },
}

#[derive(Resource)]
struct TextureAtlases {
    dice: Handle<TextureAtlas>,
    explosion: Handle<TextureAtlas>,
    items: Handle<TextureAtlas>,
}

#[derive(Component, Serialize, Deserialize, Default)]
struct Dice {
    value: u8,
}

#[derive(Bundle, Default)]
struct DiceBundle {
    dice: Dice,
    replication: Replication,
}

#[derive(Event, Serialize, Deserialize)]
struct DiceRollRequest;

#[derive(Event, Serialize, Deserialize)]
enum MoveRequest {
    Up,
    Down,
    Left,
    Right,
}

impl MoveRequest {
    fn delta(&self) -> IVec2 {
        match self {
            MoveRequest::Up => IVec2::Y,
            MoveRequest::Down => IVec2::NEG_Y,
            MoveRequest::Left => IVec2::NEG_X,
            MoveRequest::Right => IVec2::X,
        }
    }
}

#[derive(Resource)]
struct Maze {
    horizontal_bars: [[bool; 6]; 5],
    vertical_bars: [[bool; 5]; 6],
}

impl Maze {
    fn generate(num_tiles: u8) -> Maze {
        let mut maze = Maze {
            horizontal_bars: [[false; 6]; 5],
            vertical_bars: [[false; 5]; 6],
        };

        let mut rng = rand::thread_rng();
        for _ in 0..num_tiles {
            loop {
                if rng.gen::<bool>() {
                    let x = rng.gen_range(0..6);
                    let y = rng.gen_range(0..5);
                    if !maze.horizontal_bars[y][x] {
                        maze.horizontal_bars[y][x] = true;
                        if maze.is_valid() {
                            break;
                        }
                        maze.horizontal_bars[y][x] = false;
                    }
                } else {
                    let x = rng.gen_range(0..5);
                    let y = rng.gen_range(0..6);
                    if !maze.vertical_bars[y][x] {
                        maze.vertical_bars[y][x] = true;
                        if maze.is_valid() {
                            break;
                        }
                        maze.vertical_bars[y][x] = false;
                    }
                }
            }
        }

        maze
    }

    fn is_valid(&self) -> bool {
        let mut reachable = [[false; 6]; 6];
        self.dfs(IVec2::ZERO, &mut reachable);
        reachable.iter().flatten().all(|b| *b)
    }

    fn dfs(&self, pos: IVec2, reachable: &mut [[bool; 6]; 6]) {
        reachable[pos.y as usize][pos.x as usize] = true;
        if pos.x != 0 {
            let next_pos = pos + IVec2::NEG_X;
            if !reachable[next_pos.y as usize][next_pos.x as usize]
                && !self.is_blocked(pos, next_pos)
            {
                self.dfs(next_pos, reachable);
            }
        }
        if pos.x != 5 {
            let next_pos = pos + IVec2::X;
            if !reachable[next_pos.y as usize][next_pos.x as usize]
                && !self.is_blocked(pos, next_pos)
            {
                self.dfs(next_pos, reachable);
            }
        }
        if pos.y != 0 {
            let next_pos = pos + IVec2::NEG_Y;
            if !reachable[next_pos.y as usize][next_pos.x as usize]
                && !self.is_blocked(pos, next_pos)
            {
                self.dfs(next_pos, reachable);
            }
        }
        if pos.y != 5 {
            let next_pos = pos + IVec2::Y;
            if !reachable[next_pos.y as usize][next_pos.x as usize]
                && !self.is_blocked(pos, next_pos)
            {
                self.dfs(next_pos, reachable);
            }
        }
    }

    fn is_blocked(&self, from: IVec2, to: IVec2) -> bool {
        assert_eq!(1, from.x.abs_diff(to.x) + from.y.abs_diff(to.y));
        if from.x == to.x {
            self.horizontal_bars[from.y.min(to.y) as usize][from.x as usize]
        } else {
            self.vertical_bars[from.y as usize][from.x.min(to.x) as usize]
        }
    }
}

macro_rules! items {
    ($(($name:ident @ $x:literal, $y: literal),)*) => {
        #[derive(Debug, Serialize, Deserialize, Default, Copy, Clone)]
        enum Item {
            #[default]
            $($name,)*
        }

        impl Item {
            const ALL: [Item; 24] = [$(Item::$name,)*];

            fn coords(&self) -> IVec2 {
                match self {
                    $(Item::$name => IVec2::new($x, $y),)*
                }
            }
        }

        impl std::fmt::Display for Item {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(Item::$name => f.write_str(stringify!($name)),)*
                }
            }
        }
    }
}

items! {
    (Bracelet @ 2, 0),
    (YinYang @ 3, 0),
    (Lightning @ 1, 1),
    (Moon @ 2, 1),
    (ShootingStar @ 3, 1),
    (Fire @ 4, 1),
    (Bird @ 0, 2),
    (Dagger @ 1, 2),
    (Crown @ 2, 2),
    (Mushroom @ 3, 2),
    (Ring @ 4, 2),
    (Mouse @ 5, 2),
    (Sun @ 0, 3),
    (Snake @ 1, 3),
    (Flower @ 2, 3),
    (Candle @ 3, 3),
    (Feather @ 4, 3),
    (Cat @ 5, 3),
    (SpiderWeb @ 1, 4),
    (Bat @ 2, 4),
    (Owl @ 3, 4),
    (Eye @ 4, 4),
    (PartyHat @ 2, 5),
    (MagicWand @ 3, 5),
}

impl Item {
    fn atlas_index(&self) -> usize {
        let coords = self.coords();
        (BOARD_SIZE - 1 - coords.y as usize) * BOARD_SIZE + coords.x as usize
    }
}

#[derive(Resource)]
struct AvailableItems(Vec<Item>);

impl Default for AvailableItems {
    fn default() -> Self {
        let mut vec = Vec::with_capacity(24);
        vec.extend_from_slice(&Item::ALL);
        AvailableItems(vec)
    }
}

impl AvailableItems {
    fn take_random(&mut self) -> Option<Item> {
        if self.0.is_empty() {
            None
        } else {
            let index = rand::thread_rng().gen_range(0..self.0.len());
            Some(self.0.remove(index))
        }
    }
}

enum ItemDisplayPosition {
    Achieved(usize),
    Target,
}

#[derive(Component)]
struct ItemDisplay {
    player_index: usize,
    position: ItemDisplayPosition,
}

#[derive(Bundle)]
struct ItemDisplayBundle {
    item: ItemDisplay,
    sprite: SpriteSheetBundle,
}
